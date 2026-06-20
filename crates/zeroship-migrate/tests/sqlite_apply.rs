//! Journal + atomic-apply + idempotency proofs for the SQLite migration backend
//! (design §2.2 / §2.2.2, P2 gate). Real temp-file SQLite throughout.

use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::backend::MigrationBackend;
use zeroship_migrate::db::ExecutorConfig;
use zeroship_migrate::journal::Phase;
use zeroship_migrate::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};
use zeroship_migrate::SqliteBackend;

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

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn mig(up: &str) -> Migration {
    mig_named("m", up)
}

fn mig_named(name: &str, up: &str) -> Migration {
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
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum,
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }
}

/// A throwaway ExecutorConfig (the SQLite backend ignores PG-shaped fields).
fn cfg() -> ExecutorConfig {
    ExecutorConfig::new("prj_test", "app")
}

// ---------------------------------------------------------------------------
// Apply one engine-generated CREATE TABLE → table exists; journal row completed;
// re-run is a no-op (idempotent).
// ---------------------------------------------------------------------------
#[compio::test]
async fn apply_create_table_then_idempotent_rerun() {
    let p = paths("apply1");
    let be = backend(&p);
    let m = mig("CREATE TABLE app.users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);");

    // First apply: newly applied.
    let applied = be
        .apply_one_additive(&m, "deployer")
        .await
        .expect("apply create table");
    assert!(applied, "first apply must report newly-applied");

    // The table exists in the app file.
    let rows = be
        .actor()
        .query("SELECT name FROM app.sqlite_master WHERE type='table' AND name='users'")
        .await
        .expect("introspect app schema");
    assert_eq!(rows.len(), 1, "users table must exist after apply");

    // The journal has a `completed` row for this version.
    let net = be.applied_sqlite().await.expect("read journal");
    let v = m.version.as_str();
    let entry = net
        .iter()
        .find(|e| e.version == v)
        .expect("version must be journaled");
    assert_eq!(entry.phase, Phase::Completed);
    assert_eq!(entry.checksum, m.checksum.as_str());

    // Re-run: idempotent no-op (returns false), and there is still exactly ONE
    // completed row (no double-apply).
    let again = be
        .apply_one_additive(&m, "deployer")
        .await
        .expect("re-run is a no-op");
    assert!(!again, "re-run must report no-op");
    let count_rows = be
        .actor()
        .query(&format!(
            "SELECT COUNT(*) FROM \"_mig\".schema_migrations WHERE version = '{v}'"
        ))
        .await
        .expect("count journal rows");
    assert_eq!(
        count_rows[0][0].as_deref(),
        Some("1"),
        "exactly one completed row after re-run"
    );
}

// ---------------------------------------------------------------------------
// Two migrations get monotonically increasing, comparable event_seq from the
// SHARED counter (M4) — proving the cross-table total order.
// ---------------------------------------------------------------------------
#[compio::test]
async fn shared_event_seq_is_monotonic() {
    let p = paths("seq");
    let be = backend(&p);
    let m1 = mig_named("first", "CREATE TABLE app.a (id INTEGER);");
    let m2 = mig_named("second", "CREATE TABLE app.b (id INTEGER);");
    be.apply_one_additive(&m1, "d").await.expect("apply m1");
    be.apply_one_additive(&m2, "d").await.expect("apply m2");

    let rows = be
        .actor()
        .query("SELECT version, event_seq FROM \"_mig\".schema_migrations ORDER BY event_seq")
        .await
        .expect("read event_seq");
    assert_eq!(rows.len(), 2);
    let s0: i64 = rows[0][1].as_deref().unwrap().parse().unwrap();
    let s1: i64 = rows[1][1].as_deref().unwrap().parse().unwrap();
    assert!(s1 > s0, "event_seq must strictly increase across migrations");
    // The shared counter's `next` is now past both allocations.
    let nxt = be
        .actor()
        .query("SELECT next FROM \"_mig\".event_seq WHERE id = 1")
        .await
        .expect("read counter");
    let next: i64 = nxt[0][0].as_deref().unwrap().parse().unwrap();
    assert!(next > s1, "counter advanced past the last allocation");
}

// ---------------------------------------------------------------------------
// Immutability: direct UPDATE/DELETE on _mig.schema_migrations is rejected. On
// the Confined path the authorizer denies it FIRST (at prepare); to prove the
// trigger backstop independently we exercise it on a relaxed connection below.
// Here we assert the end-to-end Confined behavior: a creator `up` UPDATE/DELETE
// on the journal is denied and the row is untouched.
// ---------------------------------------------------------------------------
#[compio::test]
async fn journal_update_delete_denied_confined() {
    let p = paths("immut");
    let be = backend(&p);
    let m = mig("CREATE TABLE app.t (id INTEGER);");
    be.apply_one_additive(&m, "d").await.expect("apply");
    let v = m.version.as_str();

    // A creator `up` trying to UPDATE the journal — denied by the authorizer.
    let upd = mig(&format!(
        "UPDATE \"_mig\".schema_migrations SET checksum = 'tampered' WHERE version = '{v}';"
    ));
    let e = be
        .apply_one_additive(&upd, "attacker")
        .await
        .expect_err("journal UPDATE must be denied");
    assert!(e.is_authorizer_denied(), "expected authorizer deny, got {e}");

    // A creator `up` trying to DELETE the journal — denied too.
    let del = mig(&format!(
        "DELETE FROM \"_mig\".schema_migrations WHERE version = '{v}';"
    ));
    let e = be
        .apply_one_additive(&del, "attacker")
        .await
        .expect_err("journal DELETE must be denied");
    assert!(e.is_authorizer_denied(), "expected authorizer deny, got {e}");

    // The original checksum is intact.
    let net = be.applied_sqlite().await.expect("read journal");
    let entry = net.iter().find(|x| x.version == v).expect("still present");
    assert_eq!(entry.checksum, m.checksum.as_str(), "checksum untampered");
}

// ---------------------------------------------------------------------------
// The append-only TRIGGER backstop (§2.2.1 item 5) fires even when the authorizer
// is NOT in the path — proving the in-DB defense independently. We open the
// journal file with a PLAIN connection (no authorizer) and attempt UPDATE/DELETE;
// the RAISE(ABORT) trigger must reject it.
// ---------------------------------------------------------------------------
#[compio::test]
async fn journal_immutability_trigger_backstop() {
    let p = paths("trg");
    let be = backend(&p);
    let m = mig("CREATE TABLE app.t (id INTEGER);");
    be.apply_one_additive(&m, "d").await.expect("apply");
    drop(be); // close the hardened connection so the file is free.

    // Re-open the journal file directly, no authorizer — the trigger is the only
    // defense here.
    let conn = rusqlite::Connection::open(&p.journal).expect("open journal raw");
    let v = m.version.as_str();
    let upd = conn.execute_batch(&format!(
        "UPDATE schema_migrations SET checksum = 'x' WHERE version = '{v}';"
    ));
    assert!(
        upd.is_err(),
        "append-only trigger must reject UPDATE even without the authorizer"
    );
    let del = conn.execute_batch("DELETE FROM schema_migrations;");
    assert!(
        del.is_err(),
        "append-only trigger must reject DELETE even without the authorizer"
    );
}

// ---------------------------------------------------------------------------
// transaction:false on SQLite → rejected with a clear error (design §2.3/L3),
// through the trait's validate_non_txn.
// ---------------------------------------------------------------------------
#[compio::test]
async fn transaction_false_rejected() {
    let p = paths("nontxn");
    let be = backend(&p);
    let mut m = mig("CREATE TABLE app.t (id INTEGER);");
    m.flags.transactional = false;
    m.checksum = Checksum::of(&ChecksumInput::from_migration(&m));

    let err = MigrationBackend::validate_non_txn(&be, &m)
        .expect_err("transaction:false must be rejected on SQLite");
    // A clear, dialect-named error.
    let msg = err.to_string();
    assert!(
        msg.contains("sqlite") && msg.contains("non-transactional"),
        "error must name the dialect + the missing non-txn path, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// A failing creator `up` (mid-batch DDL error) rolls back atomically: no journal
// row, and the partial table from the first statement is also gone.
// ---------------------------------------------------------------------------
#[compio::test]
async fn failed_up_rolls_back_atomically() {
    let p = paths("atomic");
    let be = backend(&p);
    // First statement creates a table; the second is a hard error (creating the
    // SAME table again ⇒ "table ok already exists") ⇒ the whole transaction must
    // roll back, including the first statement's table.
    let m = mig(
        "CREATE TABLE app.ok (id INTEGER); \
         CREATE TABLE app.ok (id INTEGER);",
    );
    let res = be.apply_one_additive(&m, "d").await;
    assert!(res.is_err(), "a failing up must error");

    // No journal row for this version (the journal write never happened).
    let net = be.applied_sqlite().await.expect("read journal");
    assert!(
        !net.iter().any(|e| e.version == m.version.as_str()),
        "failed up leaves no journal row"
    );
    // The `ok` table from the first statement was rolled back too.
    let rows = be
        .actor()
        .query("SELECT name FROM app.sqlite_master WHERE type='table' AND name='ok'")
        .await
        .expect("introspect");
    assert!(rows.is_empty(), "partial DDL must be rolled back atomically");
}

// ---------------------------------------------------------------------------
// dialect() reports Sqlite through the trait.
// ---------------------------------------------------------------------------
#[compio::test]
async fn reports_sqlite_dialect() {
    let p = paths("dialect");
    let be = backend(&p);
    assert_eq!(
        MigrationBackend::dialect(&be),
        zeroship_schema::query::SqlDialect::Sqlite
    );
    // ensure_journal through the trait works and applied() is empty initially.
    let c = cfg();
    MigrationBackend::ensure_journal(&be, &c)
        .await
        .expect("ensure_journal via trait");
    let net = MigrationBackend::applied(&be, &c)
        .await
        .expect("applied via trait");
    assert!(net.is_empty(), "fresh journal is empty");
}
