//! Raw-SQL fixture tables for plugin-db's SQLite tests.
//!
//! plugin-db does not own DDL. The schema authority is a separate process
//! (`crates/zeroship-migrate-server` at deploy, the vite plugin's
//! dev apply locally). A test that wants to exercise the data plane therefore has
//! to put the table there itself, and it does so by SPELLING THE SQL, not by
//! calling a builder.
//!
//! # Why raw SQL and not a schema-derived builder
//!
//! Two earlier versions of this fixture derived the DDL: first by driving the
//! migration engine, then by calling `zeroship-data-sql`'s emitter. Both made the
//! test depend on the very layer under test to describe its own fixture, so a
//! wrong emitter produced a wrong table AND a matching expectation, and the test
//! still passed. Written out, the DDL is an independent statement of what the
//! table is, and a drift between it and the emitter shows up as a failure rather
//! than as agreement.
//!
//! The cost is that the literal and the declared schema each test registers must
//! be kept in step by hand. That is the intended trade: they are declared next to
//! each other, and a mismatch fails loudly at the first query.
//!
//! # What a table built here is NOT
//!
//! It is not a deployed creator's table. The engine renders `id` / `created_by` /
//! `updated_by` as `varchar(255)` on PostgreSQL where these literals say `text`,
//! a divergence the platform's own `[[inject]]` system-shape fragment records as
//! deliberate. A test built on these tables pins the JSON projection and the
//! dialect behaviour, not production's exact column types.
//!
//! # Why a plain connection is safe here
//!
//! The engine's hardened `SqliteBackend` exists to confine CREATOR-authored
//! migrations: an authorizer deny-list, journal immutability, ATTACH isolation. A
//! fixture creating its own table in its own tempdir, with no creator and no
//! journal, has nothing for those to protect.
//!
//! The two-connections-one-file hazard does not apply either, though the reason
//! is not the one an earlier draft of this doc gave.
//!
//! THAT DRAFT SAID the data-plane backend ATTACHes `zs-<app_id>.sqlite` "lazily,
//! on the app's first use". That is now true: the execution path calls the
//! idempotent cached ATTACH before addressing a table. The fixture still opens
//! its own connection, writes DDL, and drops it before the runtime connection
//! opens the file. See [`create_sqlite_table`].

use rusqlite::Connection;
use std::path::Path;

/// Execute `ddl` against `<db_dir>/zs-<app_id>.sqlite`.
///
/// `ddl` is raw SQL owned by the calling test. It runs with the app file
/// ATTACHed under `app_id`, which is the alias the data plane addresses, so the
/// statements should qualify their targets the way the runtime sees them
/// (`"<app_id>"."<table>"`).
///
/// Call this BEFORE the data-plane backend touches `app_id` - constructing the
/// backend is fine, the constraint is that no operation for this `app_id` has run
/// yet, because that is what triggers the ATTACH (see the module doc).
///
/// Panics on failure: a fixture that cannot build its table has nothing left to
/// assert, and a silent skip here would read as a passing test.
pub fn create_sqlite_table(db_dir: &Path, app_id: &str, ddl: &str) {
    assert!(
        !app_id.contains('"'),
        "app_id is interpolated into an ATTACH statement: {app_id}"
    );
    let file = db_dir.join(format!("zs-{app_id}.sqlite"));
    let path = file
        .to_str()
        .expect("the fixture db_dir path is UTF-8")
        .replace('\'', "''");

    let conn = Connection::open_in_memory().expect("open the fixture connection");
    conn.execute_batch(&format!("ATTACH DATABASE '{path}' AS \"{app_id}\";"))
        .expect("attach the app file under the alias the data plane addresses");
    conn.execute_batch(ddl)
        .unwrap_or_else(|e| panic!("fixture DDL failed: {e}\n{ddl}"));
}
