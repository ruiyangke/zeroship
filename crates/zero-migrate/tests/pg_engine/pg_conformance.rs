//! Live-Postgres conformance for the `PgDevSession` driver.
//!
//! Proves the in-crate `PgDevSession` — the TEST-ONLY [`SqlSession`] over the blocking
//! `postgres` crate that every resurrected `*_pg` scenario drives — passes the engine's
//! `driver::conformance` suite (session-pinning, transaction-visibility, `exec_text`
//! semantics, error+SQLSTATE mapping). It is the FIRST external consumer of the seam's
//! conformance surface, so a driver that silently pools connections, swallows errors, or
//! sends the wrong param format is caught here BEFORE any scenario relies on it.
//!
//! REQUIRES `ZERO_MIGRATE_TEST_PG_URL`. An unset DSN FAILS these tests: a skipped
//! live suite reports exactly like a passing one, so there is no skip.

use crate::support::PgDevSession;
use zero_migrate::driver::conformance::{self, SeamFixture};

/// PostgreSQL's spelling of the scratch SQL the suite runs. The checks are
/// neutral; these are not, which is why the caller owns them.
const PG_FIXTURE: SeamFixture = SeamFixture {
    temp_keyword: "TEMP",
    bigint_type: "int8",
    timestamp_type: "timestamptz",
    timestamp_text_param: "2026-01-02T03:04:05Z",
    placeholder: pg_placeholder,
    as_bigint: pg_as_bigint,
    ts_matches: pg_ts_matches,
    undefined_table_sqlstate: "42P01",
};

fn pg_placeholder(n: usize) -> String {
    format!("${n}")
}

fn pg_as_bigint(expr: &str) -> String {
    format!("({expr})::int8")
}

fn pg_ts_matches() -> String {
    "ts = timestamptz '2026-01-02T03:04:05Z'".to_string()
}

/// A unique scratch identifier per test so parallel runs never collide on the temp
/// table name (temp tables are session-scoped, but the name is still per-session).
fn scratch_ident(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    format!("zm_conf_{tag}_{n}")
}

#[compio::test]
async fn pg_dev_session_passes_seam_conformance() {
    let url = require_live_pg!();
    let session = PgDevSession::connect(&url);
    let scratch = scratch_ident("pin");
    conformance::run(&session, &scratch, &PG_FIXTURE)
        .await
        .expect("PgDevSession must pass the full driver::conformance suite");
}
