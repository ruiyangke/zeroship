//! Live-Postgres driver conformance for [`CompioPgSession`] - the seam producer
//! this service actually SHIPS.
//!
//! `zeroship_migrate_backend::driver::conformance` is the suite built to prove a
//! [`SqlSession`] driver honours the four invariants the engine's apply path relies
//! on and never re-checks: session pinning, transaction visibility, bind-inference
//! semantics, and error + SQLSTATE mapping. Until this file it was driven from two
//! places only - `crates/zeroship-migrate/tests/pg_engine/pg_conformance.rs` against
//! the test harness `PgDevSession`, and the `MySQL` sibling against `MysqlDevSession`.
//! Both are TEST drivers. The driver that applies creator DDL in production, this
//! crate's `CompioPgSession`, was never run through it. `session.rs`'s own doc said
//! so: "NOTHING IN THE TREE HOLDS THAT".
//!
//! The gap is not cosmetic, and `to_holder`'s comment in `src/session.rs` names why:
//! [`Bind`] is `#[non_exhaustive]`, so an out-of-crate `match` can never be
//! exhaustive and the compiler will NEVER warn this driver that a variant arrived
//! unhandled. That is exactly how `Bind::Inferred` went missing once already - the
//! enum grew, the driver did not, the build stayed green, and every inferred
//! parameter failed at runtime. Invariant 3 of this suite is the only thing that
//! catches it.
//!
//! `PgDevSession` cannot stand in for that. It reaches the server over the BLOCKING
//! `postgres` crate and expresses "declare nothing" through a `ToSql` impl whose
//! `accepts` returns true; `CompioPgSession` reaches it over compio/`io_uring` and
//! expresses the same thing by pairing each holder with an explicit `Type::UNKNOWN`
//! through `execute_typed` / `query_typed`. Different client, different protocol
//! path, different mechanism for the one invariant that has already broken once.
//!
//! # Proving the suite reached the driver under test
//!
//! A conformance target that conforms an object you did not mean to test is worse
//! than no target. Three independent facts are asserted around the run, and none of
//! them can hold for a `PgDevSession`:
//!
//! 1. The backend pid read through the SEAM (`SqlSession::query_one`) equals the one
//!    read through the RAW [`compio_postgres::Client`] borrowed from
//!    [`CompioPgSession::client`]. That probe only compiles against a compio client,
//!    so the object under test is a compio-postgres connection by construction, and
//!    the equality proves the seam's verbs land on that same physical backend.
//! 2. An INDEPENDENT connection finds that pid in `pg_stat_activity` carrying the
//!    distinctive `application_name` this test connected with. An outside observer,
//!    not the seam, confirms which backend we hold.
//! 3. `pg_my_temp_schema()` is 0 on the seam BEFORE the run and non-zero AFTER it.
//!    `PostgreSQL` creates a session's temp schema lazily, on the first temp object,
//!    and keeps it for the life of the session. So this is a durable, deterministic
//!    receipt that the suite's `CREATE TEMP TABLE`s executed on THIS backend - the
//!    scratch tables themselves are dropped by the suite and leave nothing to look at.
//!
//! # Gating
//!
//! `required-features = ["live-db-tests"]` (see `Cargo.toml`), the same contract
//! `apply_api_test` carries: a database-free build never compiles this target. When
//! the feature IS on and no DSN is configured, this test PANICS naming the fixture
//! that provisions one. It does not skip - a skipped live suite reports exactly like
//! a passing one, which is the whole reason this gap survived unnoticed.

use zeroship_migrate::driver::conformance::{self, SeamFixture};
use zeroship_migrate::driver::SqlSession;
use zeroship_migrate_server::session::CompioPgSession;

/// `PostgreSQL`'s spelling of the scratch SQL the suite runs. Byte-identical to the
/// fixture `crates/zeroship-migrate/tests/pg_engine/pg_conformance.rs` hands the
/// harness driver: the dialect is the same server, so any difference here would be
/// this target quietly testing something easier.
const PG_FIXTURE: SeamFixture = SeamFixture {
    temp_keyword: "TEMP",
    bigint_type: "int8",
    bool_type: "boolean",
    decimal_type: "numeric(40,10)",
    timestamp_type: "timestamptz",
    timestamp_text_param: "2026-01-02T03:04:05Z",
    placeholder: pg_placeholder,
    as_bigint: pg_as_bigint,
    as_text: pg_as_text,
    ts_matches: pg_ts_matches,
    undefined_table_sqlstate: "42P01",
};

fn pg_placeholder(n: usize) -> String {
    format!("${n}")
}

fn pg_as_bigint(expr: &str) -> String {
    format!("({expr})::int8")
}

fn pg_as_text(expr: &str) -> String {
    format!("({expr})::text")
}

fn pg_ts_matches() -> String {
    "ts = timestamptz '2026-01-02T03:04:05Z'".to_string()
}

/// A unique scratch identifier so parallel runs never collide on the temp-table
/// name (temp tables are session-scoped, but the name is still per-session).
fn scratch_ident(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("zm_conf_{tag}_{pid}_{n}")
}

/// The DSN, or a loud refusal. There is no skip arm on purpose: this target exists
/// because a driver went unexercised for a whole release cycle while every run
/// reported green, and a skip would rebuild exactly that.
fn require_live_pg() -> String {
    zeroship_core::config::test_database_url_opt().unwrap_or_else(|| {
        panic!(
            "compio_pg_conformance REQUIRES a live PostgreSQL: no test DSN configured. \
             Set PG_TEST_URL to a DSN on :5440, or run tests/provision_test_backends.sh. \
             This test does not skip - a skipped live suite reports exactly like a \
             passing one, which is how CompioPgSession went unconformed."
        )
    })
}

/// The config for the session under test, carrying a distinctive
/// `application_name` so an independent observer can identify the backend from
/// outside.
///
/// The DSN is round-tripped through a parsed `Config` rather than edited as a
/// string: a Postgres DSN admits percent-encoded userinfo, `?`-query parameters and
/// comma-separated multi-host lists, so string surgery on it can silently corrupt a
/// password.
fn tagged_config(url: &str, app_name: &str) -> compio_postgres::Config {
    let mut config: compio_postgres::Config = url.parse().expect("test DSN parses as a PG config");
    config.application_name(app_name);
    config
}

/// Read `pg_backend_pid()` through the neutral seam verbs.
async fn seam_backend_pid(session: &CompioPgSession) -> i64 {
    let row = session
        .query_one("SELECT (pg_backend_pid())::int8 AS pid", &[])
        .await
        .expect("read backend pid over the SqlSession seam");
    row.try_get("pid").expect("decode seam backend pid")
}

/// Read `pg_backend_pid()` through the RAW compio client.
///
/// This is the load-bearing half of evidence 1: `CompioPgSession::client` hands back
/// a `&compio_postgres::Client`, and this call is typed against that client's own
/// API. It cannot compile against any other driver, so a session that satisfied it
/// IS a compio-postgres connection.
async fn raw_backend_pid(session: &CompioPgSession) -> i64 {
    let row = session
        .client()
        .query_one("SELECT (pg_backend_pid())::int8 AS pid", &[])
        .await
        .expect("read backend pid over the raw compio_postgres client");
    row.get::<_, i64>("pid")
}

/// `pg_my_temp_schema()` - 0 until this session creates its first temp object, and
/// stable for the rest of the session once it has. Read over the seam.
async fn seam_temp_schema_oid(session: &CompioPgSession) -> i64 {
    let row = session
        .query_one("SELECT (pg_my_temp_schema())::int8 AS ts", &[])
        .await
        .expect("read pg_my_temp_schema over the seam");
    row.try_get("ts").expect("decode temp schema oid")
}

#[compio::test]
async fn compio_pg_session_passes_seam_conformance() {
    let url = require_live_pg();
    let app_name = format!("zs-seam-conformance-{}", std::process::id());
    let session = CompioPgSession::connect_with_config(&tagged_config(&url, &app_name))
        .await
        .expect("connect CompioPgSession to the test PG");

    // --- Evidence 1: the seam and the raw compio client are ONE backend. ---
    let seam_pid = seam_backend_pid(&session).await;
    let raw_pid = raw_backend_pid(&session).await;
    assert_eq!(
        seam_pid, raw_pid,
        "the SqlSession verbs and the borrowed compio_postgres::Client reported \
         different backend pids - the seam is not running on the client this test holds"
    );

    // --- Evidence 2: an INDEPENDENT connection identifies that backend. ---
    let observer = CompioPgSession::connect(&url)
        .await
        .expect("connect the independent observer session");
    let observer_pid = seam_backend_pid(&observer).await;
    assert_ne!(
        observer_pid, seam_pid,
        "the observer landed on the same backend as the session under test - it is \
         not an independent connection"
    );
    let seen = observer
        .client()
        .query_one(
            "SELECT application_name, backend_type FROM pg_stat_activity WHERE pid = $1",
            &[&i32::try_from(seam_pid).expect("backend pid fits in i32")],
        )
        .await
        .expect("observe the session under test in pg_stat_activity");
    assert_eq!(
        seen.get::<_, String>("application_name"),
        app_name,
        "pg_stat_activity does not show the application_name this test connected with"
    );
    assert_eq!(
        seen.get::<_, String>("backend_type"),
        "client backend",
        "the observed pid is not an ordinary client backend"
    );

    // --- Evidence 3, part one: no temp schema exists on this session YET. ---
    let temp_before = seam_temp_schema_oid(&session).await;
    assert_eq!(
        temp_before, 0,
        "this session already had a temp schema before the conformance run; the \
         after-check below would then prove nothing"
    );

    // --- The suite itself, against the driver that ships. ---
    let scratch = scratch_ident("pin");
    conformance::run(&session, &scratch, &PG_FIXTURE)
        .await
        .expect("CompioPgSession must pass the full driver::conformance suite");

    // --- Evidence 3, part two: the suite's temp objects were created HERE. ---
    let temp_after = seam_temp_schema_oid(&session).await;
    assert_ne!(
        temp_after, 0,
        "pg_my_temp_schema() is still 0 after the conformance run - no CREATE TEMP \
         TABLE ever executed on the backend this test holds, so the suite ran \
         somewhere else"
    );
}
