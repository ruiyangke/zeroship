//! Live-MySQL conformance for the `MysqlDevSession` driver.
//!
//! The seam's invariants - session pinning, transaction visibility, `exec` versus
//! bind-inference semantics, error surfacing - are things the apply path RELIES
//! on and never re-checks. `driver::conformance` exists to prove a driver honours
//! them, and until this file it was run against PostgreSQL alone, even though its
//! own module doc named the `mysql2` shell among the drivers it covers.
//!
//! Pinning is the one that most needs a second dialect. The suite's own header
//! spells out why: a driver that silently round-robins a pool passes a recording
//! smoke test and then corrupts a real apply, because the `BEGIN` lands on one
//! backend and the `COMMIT` on another. MySQL reaches the server through a
//! different client with its own pooling, so "PostgreSQL pins correctly" says
//! nothing about it.
//!
//! REQUIRES `ZERO_MIGRATE_MYSQL_URL`. An unset DSN FAILS these tests: a skipped
//! live suite reports exactly like a passing one, so there is no skip.

use crate::support::mysql::MysqlDevSession;
use zeroship_migrate::driver::conformance::{self, SeamFixture};

/// MySQL's spelling of the scratch SQL the suite runs.
///
/// Every difference from the PostgreSQL fixture is a real grammar difference, not
/// a preference: `TEMPORARY` rather than `TEMP`, `BIGINT` rather than `int8`,
/// `?` rather than `$N`, `CAST(... AS SIGNED)` rather than `::int8`,
/// `CAST(... AS CHAR)` rather than `::text`, a space rather than `T` in the
/// timestamp literal and no trailing zone designator, and SQLSTATE `42S02` rather
/// than `42P01` for a missing table.
///
/// `BOOLEAN` is the sharpest of them: MySQL accepts the keyword and stores a
/// `TINYINT(1)`, so there is no boolean type here at all. The suite asserts only
/// what survives that - a bound `Bind::Bool` selects the row whose flag equals it -
/// which is true of a `TINYINT` and of PostgreSQL's real `boolean` alike.
const MYSQL_FIXTURE: SeamFixture = SeamFixture {
    temp_keyword: "TEMPORARY",
    bigint_type: "BIGINT",
    bool_type: "BOOLEAN",
    decimal_type: "DECIMAL(40,10)",
    timestamp_type: "DATETIME(6)",
    timestamp_text_param: "2026-01-02 03:04:05",
    placeholder: mysql_placeholder,
    as_bigint: mysql_as_bigint,
    as_text: mysql_as_text,
    ts_matches: mysql_ts_matches,
    undefined_table_sqlstate: "42S02",
};

/// MySQL binds positionally in statement order, so every placeholder is `?` and
/// the index is carried by position rather than spelled.
fn mysql_placeholder(_n: usize) -> String {
    "?".to_string()
}

fn mysql_as_bigint(expr: &str) -> String {
    format!("CAST({expr} AS SIGNED)")
}

fn mysql_as_text(expr: &str) -> String {
    format!("CAST({expr} AS CHAR)")
}

fn mysql_ts_matches() -> String {
    "ts = '2026-01-02 03:04:05'".to_string()
}

/// A unique scratch identifier per test so parallel runs never collide on the
/// temp table name (temp tables are session-scoped, but the name is still
/// per-session).
fn scratch_ident(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    format!("zm_conf_{tag}_{n}")
}

#[compio::test]
async fn mysql_dev_session_passes_seam_conformance() {
    let url = require_live_mysql!();
    let session = MysqlDevSession::connect(&url);
    let scratch = scratch_ident("pin");
    conformance::run(&session, &scratch, &MYSQL_FIXTURE)
        .await
        .expect("MysqlDevSession must pass the full driver::conformance suite");
}
