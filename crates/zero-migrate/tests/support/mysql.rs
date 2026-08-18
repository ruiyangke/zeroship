//! Live-MySQL support for the in-crate Rust tests: [`MysqlDevSession`] plus the
//! database guard and env gate that go with it.
//!
//! **Why this file exists.** Before it, MySQL had NO live Rust coverage at all.
//! `ZERO_MIGRATE_MYSQL_URL` appeared in 52 files under `packages/` and in exactly
//! one file under `crates/` - `apply/backend/mysql/mod.rs`, which is SOURCE, not a
//! test. So the MySQL backend was exercised at RENDER level (unit tests over the
//! emitted SQL text) and at CLI/HOST level (TypeScript over `mysql2`), and nowhere
//! in between. No Rust test had ever asked a live MySQL server what the engine
//! actually did, and the CI `rust` job was given a PostgreSQL service only.
//!
//! That gap was not hypothetical. The charter-injected column collation defect was
//! wrong on PostgreSQL AND MySQL; the PostgreSQL half was caught by a live
//! row-ordering test and the MySQL half was pinned only at DDL-TEXT level.
//!
//! [`MysqlDevSession`] is the exact sibling of [`PgDevSession`](super::PgDevSession):
//! a TEST-ONLY [`SqlSession`] over the BLOCKING `mysql` crate, pinned to ONE
//! connection, that drives the SHIPPED generic
//! [`MysqlBackend`](zero_migrate::apply::backend::MysqlBackend) / executor / journal
//! / drift path through the SAME seam the production `mysql2` host rides.
//!
//! **Never ships.** `mysql` is a `[dev-dependency]`; `cargo tree -p zero-migrate -e
//! normal` stays free of it.
//!
//! # The decode contract is COPIED from `driver-mysql2.ts`, not invented
//!
//! A dev session whose cell mapping differs from the production host's would test a
//! driver nothing ships. Every arm of [`cell_to_value`] mirrors `valueToCell` in
//! `packages/zero-migrate-cli/src/driver-mysql2.ts`, including the two choices that
//! look odd out of context:
//!
//! - `LONGLONG` (8) and `NEWDECIMAL` (246) cross as the EXACT decimal STRING, which
//!   the napi marshaller then parses into [`Value::Int`]. That is what
//!   `supportBigNumbers` + `bigNumberStrings` do on the `mysql2` side, and it is why
//!   `event_seq` / `version` survive as exact integers.
//! - Everything text-ish crosses as [`Value::Text`]; the engine's MySQL SQL
//!   `CAST(... AS CHAR)`s the timestamps it reads, so a raw temporal cell is not on
//!   the path the engine drives. The temporal arms below exist so that an
//!   ad-hoc probe query in a test does not silently decode to the wrong variant.
//!
//! One deliberate divergence, and it only ever fires where the host would have
//! FAILED: `mysql2` maps `NEWDECIMAL` to an int cell unconditionally, so a genuine
//! fractional decimal reaches the marshaller as `"1.5"` and errors there
//! (`int cell 'intStr' not an i64`). This session falls back to
//! [`Value::Decimal`] instead of erroring. Nothing the engine reads on MySQL is a
//! fractional decimal today, so the two agree everywhere the engine looks.

#![allow(dead_code)] // Not every test binary uses every helper.

use std::cell::RefCell;

use mysql::consts::ColumnType;
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, Value as MyValue};

use zero_migrate::driver::{Bind, DbError, Row, SqlSession, Value};

/// The env var gating the live-MySQL tests. When unset, every live MySQL test skips
/// cleanly through [`announce_live_db_skip`](super::announce_live_db_skip); when set
/// to a DSN, the suite runs against it.
///
/// The SAME name the TypeScript host suite and the CI `host` job already use, so a
/// developer or a workflow that exports one DSN lights up both layers.
pub const MYSQL_URL_ENV: &str = "ZERO_MIGRATE_MYSQL_URL";

/// Read the live-MySQL DSN from [`MYSQL_URL_ENV`], or `None` when unset (-> skip).
///
/// Expects the `mysql://user:password@host:port/database` URL form; the `mysql`
/// crate's `Opts::from_url` parses it.
#[must_use]
pub fn mysql_url() -> Option<String> {
    std::env::var(MYSQL_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Yield the live-MySQL DSN, or announce the skip and return from the calling test
/// when [`MYSQL_URL_ENV`] is unset.
///
/// Routes through the SAME [`announce_live_db_skip`](crate::support::announce_live_db_skip)
/// the PostgreSQL gate uses, so `ZERO_MIGRATE_REQUIRE_LIVE_DB` turns a missing MySQL
/// DSN into a FAILURE rather than a green run with no coverage. A skip must never
/// read as a pass.
#[macro_export]
macro_rules! skip_if_no_mysql {
    () => {{
        match $crate::support::mysql::mysql_url() {
            Some(url) => url,
            None => {
                $crate::support::announce_live_db_skip($crate::support::mysql::MYSQL_URL_ENV);
                return;
            }
        }
    }};
}

/// Quote a MySQL identifier with backticks, doubling any embedded backtick.
#[must_use]
pub fn quote_ident(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

/// A process-unique, MySQL-legal database-name stem for one test.
///
/// MySQL caps a database name at 64 characters, so this stays short: `zm` plus the
/// pid, a monotonic counter, and the caller's suffix.
#[must_use]
pub fn database_token(suffix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "zm_{}_{}_{suffix}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    );
    // 64 is MySQL's hard limit, and the engine appends `_migrations` (11 chars) to
    // build the meta database name, so the stem has to fit in 53.
    name.chars().take(53).collect()
}

/// A TEST-ONLY [`SqlSession`] over a blocking `mysql::Conn`, pinned to ONE
/// connection (the seam's single-connection contract).
///
/// `RefCell` for the same reason [`PgDevSession`](super::PgDevSession) uses one: the
/// seam is `&self`, the client methods are `&mut self`, and the harness is
/// single-threaded. A temp table, a `USE`, or an open transaction created by one verb
/// is visible to the next, which is exactly what the two-phase MySQL apply relies on.
#[derive(Debug)]
pub struct MysqlDevSession {
    conn: RefCell<Conn>,
    /// The DSN this session connected with, kept so cleanup that cannot use the
    /// pinned connection can open its own.
    dsn: String,
}

impl MysqlDevSession {
    /// Connect a fresh pinned session to `dsn` and apply the SAME session `sql_mode`
    /// pin the production host applies at connection creation.
    ///
    /// The pin is `MYSQL_SESSION_SQL_MODE_PIN` in `driver-mysql2.ts`. It is applied
    /// here for the same reason it is applied there: every host-side parameterized
    /// verb must share one set of literal semantics from its first request onward,
    /// and `NO_AUTO_VALUE_ON_ZERO` is the fail-safe that keeps a legacy identity `0`
    /// a `0` instead of an implicit AUTO_INCREMENT allocation. A dev session without
    /// it would be a different driver from the one that ships.
    ///
    /// # Panics
    /// Panics if the connection fails - a test-support harness, so a connect failure
    /// is a setup error (the caller skips via [`skip_if_no_mysql!`] when the DSN is
    /// simply absent).
    #[must_use]
    pub fn connect(dsn: &str) -> Self {
        let opts =
            Opts::from_url(dsn).unwrap_or_else(|e| panic!("MysqlDevSession: parse DSN {dsn}: {e}"));
        let mut conn = Conn::new(opts)
            .unwrap_or_else(|e| panic!("MysqlDevSession: connect to {dsn} failed: {e}"));
        conn.query_drop(
            "SET SESSION sql_mode = CONCAT_WS(',', @@SESSION.sql_mode, \
             'NO_BACKSLASH_ESCAPES', 'NO_AUTO_VALUE_ON_ZERO')",
        )
        .unwrap_or_else(|e| panic!("MysqlDevSession: pin the host sql_mode: {e}"));
        Self {
            conn: RefCell::new(conn),
            dsn: dsn.to_string(),
        }
    }

    /// The DSN this session connected with.
    #[must_use]
    pub fn dsn(&self) -> &str {
        &self.dsn
    }

    /// Read the server version string, so a test can state the server it measured.
    ///
    /// # Panics
    /// Panics when the query or its decode fails.
    #[must_use]
    pub fn server_version(&self) -> String {
        self.conn
            .borrow_mut()
            .query_first::<String, _>("SELECT VERSION()")
            .unwrap_or_else(|e| panic!("MysqlDevSession: read VERSION(): {e}"))
            .unwrap_or_else(|| panic!("MysqlDevSession: VERSION() returned no row"))
    }
}

/// Drops the databases a live-MySQL test created, on the way out of scope, whether
/// the test returned or unwound.
///
/// The MySQL sibling of [`SchemaGuard`](super::SchemaGuard), and it exists for the
/// same measured reason: a `DROP DATABASE` written as the last statement of a test
/// only runs when the test reaches it, and every assert before it is a point where a
/// failing run abandons a database on the server forever. The live MySQL container
/// this was developed against had accumulated over 200 leaked
/// `*_migrations` databases from the TypeScript host suite, which is what that leak
/// looks like after a few months.
///
/// `must_use` sits on the TYPE, not only on the constructor, so a caller that drops
/// the guard on the spot does not slip past it.
#[must_use = "bind the guard to a name; it drops the databases when it falls out of scope"]
#[derive(Debug)]
pub struct DatabaseGuard<'a> {
    session: &'a MysqlDevSession,
    databases: Vec<String>,
}

impl<'a> DatabaseGuard<'a> {
    /// Take responsibility for dropping `databases` when the guard goes out of scope.
    ///
    /// Each name is guarded together with its `<name>_migrations` meta database: the
    /// engine creates that one itself on the first `ensure_journal`, so a guard over
    /// the project database alone would leak the meta database on every run.
    #[must_use = "the guard drops the databases when it falls out of scope"]
    pub fn arm<I, S>(session: &'a MysqlDevSession, databases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let databases = databases
            .into_iter()
            .flat_map(|name| {
                let name: String = name.into();
                let meta = format!("{name}_migrations");
                [name, meta]
            })
            .collect();
        Self { session, databases }
    }

    fn drop_sql(&self) -> String {
        use std::fmt::Write as _;
        let mut sql = String::new();
        for database in &self.databases {
            let _ = write!(sql, "DROP DATABASE IF EXISTS {};", quote_ident(database));
        }
        sql
    }
}

impl Drop for DatabaseGuard<'_> {
    fn drop(&mut self) {
        use std::io::Write as _;

        if self.databases.is_empty() {
            return;
        }
        let sql = self.drop_sql();

        // `try_borrow_mut`, never `borrow_mut`: a panic raised while the seam holds
        // the connection leaves the cell borrowed, and a panicking `Drop` during an
        // unwind aborts the process instead of failing the test.
        if let Ok(mut conn) = self.session.conn.try_borrow_mut() {
            // An unwind can land inside an open transaction. Outside one this is a
            // no-op warning and nothing else.
            let _ = conn.query_drop("ROLLBACK");
            if conn.query_drop(&sql).is_ok() {
                return;
            }
        }

        // The pinned connection could not do it. A second connection is opened with
        // its own lock budget so a lock the test still holds becomes a reported error
        // rather than a suite that hangs here.
        let fallback = Opts::from_url(&self.session.dsn)
            .map_err(|e| e.to_string())
            .and_then(|opts| Conn::new(opts).map_err(|e| e.to_string()))
            .and_then(|mut conn| {
                conn.query_drop("SET SESSION lock_wait_timeout = 5")
                    .and_then(|()| conn.query_drop(&sql))
                    .map_err(|e| e.to_string())
            });
        if let Err(e) = fallback {
            // Straight to the process stderr handle: libtest captures the print
            // macros, so a cleanup failure that only shows under `--nocapture` is the
            // silent leak this guard exists to end. Never a panic - see above.
            let _ = writeln!(
                std::io::stderr(),
                "DatabaseGuard: {} left behind on {}: {e}",
                self.databases.join(", "),
                self.session.dsn
            );
        }
    }
}

/// Map a `mysql::Error` -> the neutral [`DbError`], carrying the real SQLSTATE when
/// the server supplied one, so the seam's error contract sees the true DB error.
fn to_db_error(error: &mysql::Error) -> DbError {
    match error {
        mysql::Error::MySqlError(server) => DbError {
            message: server.message.clone(),
            sqlstate: Some(server.state.clone()),
        },
        other => DbError::message(other.to_string()),
    }
}

/// [`Bind`] -> a `mysql` parameter value.
///
/// Mirrors `cellsToParams` in `driver-mysql2.ts`: text and decimal cross as strings,
/// an int crosses as an exact integer token (never a quoted string, which is invalid
/// in syntax-sensitive positions such as `LIMIT ?`), and a bool crosses as MySQL's
/// 1/0.
fn bind_to_value(bind: &Bind) -> MyValue {
    match bind {
        Bind::Null => MyValue::NULL,
        Bind::Bool(b) => MyValue::Int(i64::from(*b)),
        Bind::Int(n) => MyValue::Int(*n),
        Bind::Decimal(s) | Bind::Text(s) => MyValue::Bytes(s.as_bytes().to_vec()),
        // `Bind` is `#[non_exhaustive]`; a future variant maps to SQL NULL rather
        // than panicking, exactly as the host driver's `default` arm does.
        _ => MyValue::NULL,
    }
}

fn bind_params(binds: &[Bind]) -> mysql::Params {
    if binds.is_empty() {
        mysql::Params::Empty
    } else {
        mysql::Params::Positional(binds.iter().map(bind_to_value).collect())
    }
}

/// Decode ONE cell into the neutral [`Value`], reproducing `valueToCell` in
/// `driver-mysql2.ts` arm for arm. See this module's header for the one divergence.
fn cell_to_value(value: &MyValue, column_type: ColumnType) -> Value {
    match value {
        MyValue::NULL => Value::Null,
        MyValue::Int(n) => exact_int(&n.to_string(), column_type),
        MyValue::UInt(n) => exact_int(&n.to_string(), column_type),
        MyValue::Bytes(raw) => {
            let text = String::from_utf8_lossy(raw).into_owned();
            // BIGINT / DECIMAL arrive as bytes over the text protocol and as the
            // exact string the host driver's `bigNumberStrings` produces. Route them
            // through the same exact-integer arm so the two drivers agree.
            if matches!(
                column_type,
                ColumnType::MYSQL_TYPE_LONGLONG
                    | ColumnType::MYSQL_TYPE_NEWDECIMAL
                    | ColumnType::MYSQL_TYPE_DECIMAL
            ) {
                exact_int(&text, column_type)
            } else {
                Value::Text(text)
            }
        }
        // `mysql2` returns a JS `number` for FLOAT/DOUBLE and `valueToCell` funnels
        // every `number` into an int cell, so the host loses the fraction here too.
        // Matched rather than improved: the engine reads no float column on MySQL,
        // and a dev driver that decoded one BETTER than the shipped host would hide
        // a host defect rather than expose it.
        MyValue::Float(f) => Value::Int(*f as i64),
        MyValue::Double(f) => Value::Int(*f as i64),
        // The engine `CAST(... AS CHAR)`s every timestamp it reads on MySQL, so these
        // are reached only by an ad-hoc probe query in a test. Rendered as text so
        // such a probe decodes to a readable variant instead of a wrong one.
        MyValue::Date(year, month, day, hour, minute, second, micros) => {
            Value::Text(if *micros == 0 {
                format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
            } else {
                format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"
                )
            })
        }
        MyValue::Time(negative, days, hours, minutes, seconds, micros) => {
            let sign = if *negative { "-" } else { "" };
            let hours = u32::from(*hours) + days * 24;
            Value::Text(if *micros == 0 {
                format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
            } else {
                format!("{sign}{hours:02}:{minutes:02}:{seconds:02}.{micros:06}")
            })
        }
    }
}

/// The exact-integer arm: `LONGLONG` / `NEWDECIMAL` cross as the exact decimal
/// string, which the napi marshaller parses into an `i64`.
fn exact_int(text: &str, column_type: ColumnType) -> Value {
    match text.parse::<i64>() {
        Ok(n) => Value::Int(n),
        // See the module header: `mysql2` + the marshaller would ERROR here. A
        // fractional decimal is not on any path the engine reads, so falling back to
        // the exact string keeps an ad-hoc probe readable without inventing coverage.
        Err(_) if matches!(column_type, ColumnType::MYSQL_TYPE_NEWDECIMAL) => {
            Value::Decimal(text.to_string())
        }
        Err(_) => Value::Text(text.to_string()),
    }
}

/// `mysql::Row -> driver::Row` - iterate columns, decode each cell.
fn row_to_neutral(row: &mysql::Row) -> Row {
    let columns = row.columns_ref();
    let mut names = Vec::with_capacity(columns.len());
    let mut values = Vec::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        names.push(column.name_str().into_owned());
        let cell = row.as_ref(index).unwrap_or(&MyValue::NULL);
        values.push(cell_to_value(cell, column.column_type()));
    }
    Row::new(names, values)
}

/// Drain every result set of a statement (or statement batch) and return the TOTAL
/// affected-row count.
///
/// Draining is not optional: a half-read stream would be inherited by the next verb
/// on this pinned connection, and an error raised by the SECOND statement of a batch
/// would never be reported.
///
/// The count is accumulated at each set BOUNDARY, not read once at the end, and that
/// detail was load-bearing rather than defensive. `QueryResult::affected_rows()`
/// reports the CURRENT set's OK packet, so a version of this that drained first and
/// read the count afterwards read the terminal `Done` state and returned 0 for every
/// statement. The engine noticed immediately and correctly: the first live MySQL
/// apply ever attempted from Rust failed with "mysql inflight marker insert affected
/// 0 rows; expected exactly 1". That is the engine's row-count assertion working, not
/// a MySQL defect - but it is worth recording that the assertion is real, because it
/// is the reason this helper cannot be simplified back.
fn drain_affected<T: mysql::prelude::Protocol>(
    result: &mut mysql::QueryResult<'_, '_, '_, T>,
) -> Result<u64, DbError> {
    let mut affected = 0u64;
    loop {
        affected += result.affected_rows();
        match result.iter() {
            Some(set) => {
                for row in set {
                    row.map_err(|e| to_db_error(&e))?;
                }
            }
            None => return Ok(affected),
        }
    }
}

impl SqlSession for MysqlDevSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError> {
        // The text protocol with `CLIENT_MULTI_STATEMENTS` (in the crate's default
        // capability set), which is what the seam's `batch` contract requires: one
        // `&str` that may carry `;`-separated statements.
        let mut conn = self.conn.borrow_mut();
        let mut result = conn.query_iter(sql).map_err(|e| to_db_error(&e))?;
        drain_affected(&mut result).map(|_| ())
    }

    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError> {
        let mut conn = self.conn.borrow_mut();
        let mut result = conn
            .exec_iter(sql, bind_params(binds))
            .map_err(|e| to_db_error(&e))?;
        drain_affected(&mut result)
    }

    async fn exec_text(&self, sql: &str, params: &[Option<String>]) -> Result<u64, DbError> {
        // MySQL has no equivalent of PostgreSQL's concrete-OID refusal of
        // `text -> timestamptz`, so the shipped contract makes this an ALIAS for
        // `exec` on this dialect - `driver-mysql2.ts` implements it the same way.
        let binds: Vec<Bind> = params
            .iter()
            .map(|param| param.clone().map_or(Bind::Null, Bind::Text))
            .collect();
        self.exec(sql, &binds).await
    }

    async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError> {
        let mut conn = self.conn.borrow_mut();
        let rows: Vec<mysql::Row> = conn
            .exec(sql, bind_params(binds))
            .map_err(|e| to_db_error(&e))?;
        Ok(rows.iter().map(row_to_neutral).collect())
    }

    async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError> {
        let rows = self.query(sql, binds).await?;
        match rows.len() {
            1 => Ok(rows.into_iter().next().expect("length checked")),
            n => Err(DbError::message(format!(
                "query_one expected exactly one row, got {n}"
            ))),
        }
    }
}
