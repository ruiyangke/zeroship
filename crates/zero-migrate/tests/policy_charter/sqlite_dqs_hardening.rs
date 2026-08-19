//! Pins the double-quoted-string settings of the hardened `SQLite` open sequence:
//! `SQLITE_DBCONFIG_DQS_DDL` and `SQLITE_DBCONFIG_DQS_DML`, both set to `false`.
//! EVERY claim is proven against a REAL temp-file `SQLite` opened through
//! [`SqliteBackend`] - never a bare `rusqlite` handle, because a test that builds
//! its own connection pins nothing about the sequence the engine actually runs.
//!
//! WHAT DQS DOES. `SQLite` ships a compatibility misfeature: a double-quoted token
//! that resolves to no column is silently demoted to a STRING LITERAL instead of
//! erroring. The bundled library has it ON by default - the raw-connection controls
//! below read back `DQS_DDL = true` / `DQS_DML = true` - so these two lines are the
//! only thing turning it off for the engine.
//!
//! WHY IT MATTERS. With DQS on, `CHECK ("ghost" IS NULL)` naming a column that does
//! not exist is not an error: it compiles to the constant `'ghost' IS NULL`, i.e.
//! FALSE, and `SQLite` ACCEPTS the `CREATE TABLE`. The tenant ends up with a table
//! that rejects every row, and the failure surfaces much later as a write or a
//! data-copy failure with no pointer back to the DDL that caused it. The same
//! demotion in DML turns `VALUES ("ghost")` into the stored string `'ghost'`,
//! writing wrong data with no error at all. Both controls below reproduce exactly
//! those outcomes on a DQS-on connection.
//!
//! This setting is what holds down the severity of every stale-identifier the
//! emitter could carry into DDL - `ColumnSnapshot::inline_checks` and
//! `GeneratedColumnSnapshot::expr` are two known carriers - by converting a silent
//! wrong-data outcome into a loud stop at the first statement.
//!
//! A FAILURE HERE means the engine's connection has regained DQS: emitted DDL and
//! DML naming a nonexistent column no longer fails, so a stale identifier reaches
//! the tenant's database as a constant-false CHECK or as a wrongly-stored literal.
//!
//! Every rejection assertion checks the ERROR TEXT names the offending identifier,
//! not merely that the call returned `Err`. A rejection for an unrelated reason (a
//! syntax error, a missing table, an authorizer deny, a dead actor) must NOT pass
//! these tests.
//!
//! SCOPE. `SQLITE_DBCONFIG_DEFENSIVE` and `SQLITE_DBCONFIG_TRUSTED_SCHEMA` are set
//! by the same open sequence and are pinned in `sqlite_confinement.rs`, not here.
//! Both are confinement properties - what a hostile creator `up` may reach, whose
//! failure is sandbox escape rather than wrong data - and that file's register is
//! "attack X is denied". Folding them in here would make this file's subject,
//! name, and failure message lie.

use std::path::PathBuf;

use rusqlite::config::DbConfig;
use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::actor::SqliteActorError;
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::SqliteBackend;

/// A tenant's two file paths inside a fresh temp dir.
struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(app_id: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{app_id}.sqlite"));
    let journal = dir.path().join(format!("zs-{app_id}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

/// Open the engine's OWN hardened backend and bootstrap its journal. The whole
/// point of this file is the open sequence, so nothing here constructs a
/// connection by hand.
async fn backend(p: &Paths) -> SqliteBackend {
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    be.ensure_journal_sqlite().await.expect("bootstrap journal");
    be
}

fn mig(up: &str) -> Migration {
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: "stale_identifier".to_string(),
        up: up.to_string(),
        down: None,
        checksum,
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
        effect: None,
    }
}

/// The identifier the tests name in double quotes. No table in this file has a
/// column by this name, so with DQS off every statement below must fail naming it.
const GHOST: &str = "ghost";

/// Assert a hardened-backend rejection is the DQS one and not something else.
///
/// Three separate ways this test could green-pass for the wrong reason are ruled
/// out here: an authorizer DENY (`is_authorizer_denied`), a non-statement failure
/// such as a dead actor or a poisoned connection (`Exec` match), and a syntax or
/// missing-table error that never mentions the identifier (the text check). The
/// text check is the load-bearing one: `SQLite` reports a DQS-disabled name as
/// `no such column: "ghost" - should this be a string literal in single-quotes?`.
fn assert_rejected_naming_ghost(err: &SqliteActorError, what: &str) {
    assert!(
        !err.is_authorizer_denied(),
        "{what} must be rejected by DQS name resolution, not by the authorizer: {err}"
    );
    assert!(
        matches!(err, SqliteActorError::Exec(_)),
        "{what} must fail at statement execution, not at the actor/connection level: {err}"
    );
    let text = err.to_string();
    assert!(
        text.contains(GHOST),
        "{what} must fail with an error NAMING `{GHOST}` (a syntax or missing-table \
         error would pass a bare is_err check but proves nothing): {text}"
    );
    assert!(
        text.contains("no such column"),
        "{what} must fail as an unresolved COLUMN name, which is what DQS off buys: {text}"
    );
}

/// A raw connection with DQS explicitly ON, standing in for the un-hardened
/// database the engine would be talking to if these two lines were removed. Used
/// as the positive control: it proves the hardened rejections above are caused by
/// DQS being off and not by the statements being malformed.
fn dqs_on_connection(dir: &TempDir) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(dir.path().join("control.sqlite")).expect("control open");
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, true)
        .expect("control DQS_DDL on");
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, true)
        .expect("control DQS_DML on");
    conn
}

/// A stale identifier inside an inline CHECK is REJECTED, and the error names it.
///
/// The control proves the cost of the alternative: with DQS on the very same DDL
/// is ACCEPTED, and the table it creates rejects every row it is ever offered.
#[compio::test]
async fn dqs_ddl_off_rejects_a_check_naming_no_column() {
    let p = paths("dqs_ddl");
    let be = backend(&p).await;

    let ddl = "CREATE TABLE x (a TEXT CHECK (\"ghost\" IS NULL));";
    let err = be
        .actor()
        .exec(ddl)
        .await
        .expect_err("a CHECK naming a nonexistent column must be rejected with DQS_DDL off");
    assert_rejected_naming_ghost(&err, "a CHECK naming a nonexistent column");

    // The rejection is a real stop, not a warning: no table was created.
    let rows = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='x'")
        .await
        .expect("read main schema");
    assert!(
        rows.is_empty(),
        "the rejected CREATE TABLE must not have created `x`: {rows:?}"
    );

    // Positive control. The bundled library ships DQS ON, so this is the shape the
    // engine would get without the two `set_db_config` calls.
    let dir = tempfile::tempdir().expect("control tempdir");
    let conn = dqs_on_connection(&dir);
    assert_eq!(
        conn.db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL).ok(),
        Some(true),
        "control connection must have DQS_DDL on"
    );
    conn.execute_batch(ddl).expect(
        "control: with DQS_DDL on, SQLite ACCEPTS the same CHECK -- proving the hardened \
         rejection is DQS and not a syntax error",
    );
    let poisoned = conn.execute_batch("INSERT INTO x (a) VALUES ('anything');");
    let poisoned = poisoned.expect_err("control: the DQS-accepted CHECK is constant-false");
    assert!(
        poisoned.to_string().contains("CHECK constraint failed"),
        "control: the table DQS let through rejects every row: {poisoned}"
    );
}

/// The same stale identifier carried by a real creator `up` is stopped at the
/// first statement, and leaves no journal row behind.
///
/// This is the end-to-end shape the emitter can produce: `inline_checks` and
/// `GeneratedColumnSnapshot::expr` both round-trip author text into emitted DDL.
#[compio::test]
async fn dqs_ddl_off_stops_a_creator_up_carrying_a_stale_check() {
    let p = paths("dqs_ddl_up");
    let be = backend(&p).await;

    let m =
        mig("CREATE TABLE accounts (id TEXT PRIMARY KEY, title TEXT CHECK (\"ghost\" IS NULL));");
    let err = be
        .apply_one_additive(&m, "deployer")
        .await
        .expect_err("a creator up carrying a stale CHECK identifier must not apply");
    assert_rejected_naming_ghost(&err, "a creator up carrying a stale CHECK identifier");

    let applied = be
        .applied_sqlite()
        .await
        .expect("journal still readable after the rejected up");
    let v = m.version.as_str();
    assert!(
        !applied.iter().any(|e| e.version == v),
        "a rejected up must not leave a journal row for {v}"
    );
    let rows = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='accounts'")
        .await
        .expect("read main schema");
    assert!(
        rows.is_empty(),
        "the rejected up must not have created `accounts`: {rows:?}"
    );
}

/// A double-quoted value in DML that names no column is REJECTED rather than
/// demoted to a literal.
///
/// This leg distinguishes DQS_DML off from ordinary column resolution because of
/// the control: on a DQS-on connection the identical INSERT SUCCEEDS and stores
/// the string `ghost`. Ordinary resolution cannot produce that outcome - an
/// unresolvable name is either an error (DQS off) or a literal (DQS on), and the
/// two assertions pin exactly that fork.
#[compio::test]
async fn dqs_dml_off_rejects_a_double_quoted_value_naming_no_column() {
    let p = paths("dqs_dml");
    let be = backend(&p).await;
    be.actor()
        .exec("CREATE TABLE t (a TEXT);")
        .await
        .expect("seed the target table");

    let dml = "INSERT INTO t (a) VALUES (\"ghost\");";
    let err = be
        .actor()
        .exec(dml)
        .await
        .expect_err("a double-quoted value naming no column must be rejected with DQS_DML off");
    assert_rejected_naming_ghost(&err, "a double-quoted value naming no column");

    let rows = be
        .actor()
        .query("SELECT a FROM t")
        .await
        .expect("read the target table");
    assert!(
        rows.is_empty(),
        "the rejected INSERT must not have written a row: {rows:?}"
    );

    // Positive control: with DQS_DML on the same INSERT succeeds and silently
    // stores the identifier as data. That is the wrong-data outcome this setting
    // prevents, and it is why a bare `is_err` assertion would not be enough above.
    let dir = tempfile::tempdir().expect("control tempdir");
    let conn = dqs_on_connection(&dir);
    assert_eq!(
        conn.db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML).ok(),
        Some(true),
        "control connection must have DQS_DML on"
    );
    conn.execute_batch("CREATE TABLE t (a TEXT);")
        .expect("control seed");
    conn.execute_batch(dml)
        .expect("control: with DQS_DML on, SQLite ACCEPTS the same INSERT");
    let stored: Option<String> = conn
        .query_row("SELECT a FROM t", [], |row| row.get(0))
        .expect("control read back");
    assert_eq!(
        stored.as_deref(),
        Some(GHOST),
        "control: DQS_DML on stores the identifier as a string literal"
    );
}
