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
    // UNqualified DDL lands in `main` = the app file (C1).
    let m = mig("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);");

    // First apply: newly applied.
    let applied = be
        .apply_one_additive(&m, "deployer")
        .await
        .expect("apply create table");
    assert!(applied, "first apply must report newly-applied");

    // The table exists in the app file (main).
    let rows = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='users'")
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
// C1 regression: an UNqualified `CREATE TABLE users(...)` lands in main = the
// app FILE and PERSISTS. We apply on one backend, drop it (closing the file),
// reopen a SECOND backend on the SAME paths, and prove (1) the table is visible
// in the persisted app file via a raw connection, (2) the second backend sees it
// in its own `main`, and (3) re-apply through the second backend is idempotent
// (the journal also persisted). This is the test whose ABSENCE let the
// `main = :memory:` bug hide: with an in-memory main the creator DDL went to a
// throwaway memory db, never the app file, and nothing checked persistence.
// ---------------------------------------------------------------------------
#[compio::test]
async fn unqualified_ddl_persists_to_app_file_and_reapply_idempotent() {
    let p = paths("persist");
    // ONE migration object, reused across both backends (so the version matches and
    // idempotency can be proven across a reopen).
    let m = mig("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);");
    let v = m.version.as_str().to_string();
    {
        let be = backend(&p);
        let applied = be
            .apply_one_additive(&m, "deployer")
            .await
            .expect("apply create table");
        assert!(applied, "first apply must report newly-applied");
        // The journal recorded it as completed.
        let net = be.applied_sqlite().await.expect("read journal");
        assert!(
            net.iter().any(|e| e.version == v && e.phase == Phase::Completed),
            "version must be journaled completed"
        );
        // Drop the backend here so both the app file and journal file are flushed
        // and free for a fresh open.
    }

    // (1) The table PERSISTED to the app FILE: open it with a raw, separate
    // connection (no backend, no attach) and see `users`.
    {
        let conn = rusqlite::Connection::open(&p.app).expect("reopen app file raw");
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='users'",
                [],
                |r| r.get(0),
            )
            .expect("query app file");
        assert_eq!(exists, 1, "users table must persist in the app FILE (main)");
    }

    // (2)+(3) A SECOND backend on the SAME paths sees the table in its own `main`,
    // and re-applying the same migration is an idempotent no-op (the journal also
    // persisted, so idempotency holds across a reopen).
    {
        let be2 = backend(&p);
        let rows = be2
            .actor()
            .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='users'")
            .await
            .expect("introspect reopened app schema");
        assert_eq!(rows.len(), 1, "reopened backend sees the persisted table");

        let again = be2
            .apply_one_additive(&m, "deployer")
            .await
            .expect("re-apply on reopened backend");
        assert!(!again, "re-apply across reopen must be an idempotent no-op");
    }
}

// ---------------------------------------------------------------------------
// Two migrations get monotonically increasing, comparable event_seq from the
// SHARED counter (M4) — proving the cross-table total order.
// ---------------------------------------------------------------------------
#[compio::test]
async fn shared_event_seq_is_monotonic() {
    let p = paths("seq");
    let be = backend(&p);
    let m1 = mig_named("first", "CREATE TABLE a (id INTEGER);");
    let m2 = mig_named("second", "CREATE TABLE b (id INTEGER);");
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
    let m = mig("CREATE TABLE t (id INTEGER);");
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
    let m = mig("CREATE TABLE t (id INTEGER);");
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
    let mut m = mig("CREATE TABLE t (id INTEGER);");
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
        "CREATE TABLE ok (id INTEGER); \
         CREATE TABLE ok (id INTEGER);",
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
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='ok'")
        .await
        .expect("introspect");
    assert!(rows.is_empty(), "partial DDL must be rolled back atomically");
}

// ---------------------------------------------------------------------------
// H1 regression: a failed `up` must NOT wedge the long-lived reused connection.
// Before the fix, the rollback path was `let _ = actor.exec("ROLLBACK")`, which
// swallowed any ROLLBACK failure and left BEGIN IMMEDIATE open, so the NEXT apply
// on the SAME backend failed with "cannot start a transaction within a
// transaction". Here we force a failing `up`, then run a SECOND, DIFFERENT apply
// on the SAME backend and assert it SUCCEEDS — proving the connection was cleanly
// restored to autocommit (no wedge).
// ---------------------------------------------------------------------------
#[compio::test]
async fn failed_up_does_not_wedge_next_apply_on_same_backend() {
    let p = paths("wedge");
    let be = backend(&p);

    // First: a failing up (duplicate CREATE TABLE in one batch) → must error.
    let bad = mig(
        "CREATE TABLE dup (id INTEGER); \
         CREATE TABLE dup (id INTEGER);",
    );
    let res = be.apply_one_additive(&bad, "d").await;
    assert!(res.is_err(), "the bad up must error");

    // Second: a DIFFERENT, valid migration on the SAME backend must SUCCEED — this
    // is the wedge proof. A leaked open transaction would make this fail with
    // "cannot start a transaction within a transaction".
    let good = mig_named("good_after_bad", "CREATE TABLE good (id INTEGER);");
    let applied = be
        .apply_one_additive(&good, "d")
        .await
        .expect("second apply must succeed on the same backend (no wedge)");
    assert!(applied, "second migration newly applied");

    // The good table is present; the bad one is not (its txn rolled back).
    let rows = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name IN ('good','dup') ORDER BY name")
        .await
        .expect("introspect");
    assert_eq!(rows.len(), 1, "only the good table exists");
    assert_eq!(rows[0][0].as_deref(), Some("good"));

    // And the connection is cleanly in autocommit (belt-and-suspenders on H1).
    assert!(
        be.actor().is_autocommit().await.expect("probe autocommit"),
        "connection must be back in autocommit after the failed up"
    );
}

// ---------------------------------------------------------------------------
// H1 mechanism: `is_autocommit()` — the load-bearing wedge detector the rollback
// path now consults — correctly reports an OPEN transaction as NOT autocommit and
// a clean connection as autocommit. This is the primitive the fix depends on; the
// pre-fix code swallowed the ROLLBACK result and never consulted it. We drive the
// actor directly (engine mode allows BEGIN/COMMIT) and prove the transition.
// ---------------------------------------------------------------------------
#[compio::test]
async fn is_autocommit_detects_open_transaction() {
    let p = paths("autocommit");
    let be = backend(&p);
    be.ensure_journal_sqlite().await.expect("bootstrap journal");

    // Fresh connection: autocommit.
    assert!(
        be.actor().is_autocommit().await.expect("probe"),
        "fresh connection is in autocommit"
    );

    // Open a transaction under engine mode (which the authorizer allows) — now the
    // connection is the WEDGED state the H1 fix detects.
    be.actor()
        .set_mode(zeroship_migrate::backend_sqlite::Mode::EngineJournal)
        .await
        .expect("engine mode");
    be.actor().exec("BEGIN IMMEDIATE").await.expect("begin");
    assert!(
        !be.actor().is_autocommit().await.expect("probe"),
        "an open transaction must report NOT autocommit (the wedge state)"
    );

    // Rollback restores autocommit.
    be.actor().exec("ROLLBACK").await.expect("rollback");
    assert!(
        be.actor().is_autocommit().await.expect("probe"),
        "after ROLLBACK the connection is back in autocommit"
    );
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
