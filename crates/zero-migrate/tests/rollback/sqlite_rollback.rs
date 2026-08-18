//! Additive rollback proofs for the `SQLite` migration backend.
//! Real temp-file `SQLite` throughout. Covers: additive
//! reversals (DROP TABLE / DROP COLUMN), re-pending + re-apply, the
//! rebuild-needed deferred typed error, and confinement of the `down`.

use crate::support;

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::executor::RollbackError;
use zero_migrate::apply::journal::Phase;
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::SqliteBackend;

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

/// A migration with both `up` and `down`.
fn mig(name: &str, up: &str, down: &str) -> Migration {
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: Some(down),
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
        down: Some(down.to_string()),
        checksum,
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

async fn table_exists(be: &SqliteBackend, name: &str) -> bool {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_master WHERE type='table' AND name='{name}'"
        ))
        .await
        .expect("introspect");
    !rows.is_empty()
}

// ---------------------------------------------------------------------------
// Apply CREATE TABLE → rollback via DROP TABLE: the table is gone, the journal
// has a rolled_back event, the migration is re-pending, and re-apply works.
// ---------------------------------------------------------------------------
#[compio::test]
async fn rollback_drop_table_then_repending_and_reapply() {
    let p = paths("rb_table");
    let be = backend(&p);
    let m = mig(
        "create_t",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);",
        "DROP TABLE t;",
    );
    let v = m.version.as_str().to_string();

    // Apply.
    assert!(be.apply_one_additive(&m, "d").await.expect("apply"));
    assert!(table_exists(&be, "t").await, "table exists after apply");

    // Rollback (additive).
    be.rollback_one_additive(&m, "operator")
        .await
        .expect("additive rollback via DROP TABLE");
    assert!(!table_exists(&be, "t").await, "table gone after rollback");

    // The journal has a rolled_back event for this version.
    let rb_rows = be
        .actor()
        .query(&format!(
            "SELECT version FROM \"_mig\".schema_migrations \
             WHERE event_kind='rolled_back' AND version='{v}'"
        ))
        .await
        .expect("read rolled_back");
    assert_eq!(rb_rows.len(), 1, "exactly one rolled_back event");

    // The migration is RE-PENDING: its net-state is no longer `completed` (the
    // latest event is the rolled_back, so it disappears from the completed set).
    let net = be.applied_sqlite().await.expect("net applied");
    assert!(
        !net.iter()
            .any(|e| e.version == v && e.phase == Phase::Completed),
        "version must be re-pending (not net-applied) after rollback"
    );

    // Re-apply works (the same migration runs cleanly again).
    assert!(
        be.apply_one_additive(&m, "d").await.expect("re-apply"),
        "re-apply must report newly-applied after rollback"
    );
    assert!(table_exists(&be, "t").await, "table back after re-apply");
}

// ---------------------------------------------------------------------------
// Apply ADD COLUMN → rollback via DROP COLUMN (SQLite >= 3.35): the column is
// gone after rollback.
// ---------------------------------------------------------------------------
#[compio::test]
async fn rollback_drop_column_additive() {
    let p = paths("rb_col");
    let be = backend(&p);
    // Base table; the ADD COLUMN migration below is what we roll back.
    let base = mig(
        "base",
        "CREATE TABLE people (id INTEGER PRIMARY KEY);",
        "DROP TABLE people;",
    );
    be.apply_one_additive(&base, "d").await.expect("apply base");

    // The migration that adds a column, reversible by DROP COLUMN.
    let add = mig(
        "add_nick",
        "ALTER TABLE people ADD COLUMN nickname TEXT;",
        "ALTER TABLE people DROP COLUMN nickname;",
    );
    be.apply_one_additive(&add, "d")
        .await
        .expect("apply add column");

    // Column present.
    let cols = be
        .actor()
        .query("PRAGMA main.table_info('people')")
        .await
        .expect("table_info");
    assert!(
        cols.iter()
            .any(|r| r.get(1).and_then(Clone::clone).as_deref() == Some("nickname")),
        "nickname column present after ADD COLUMN"
    );

    // Rollback via DROP COLUMN (additive, native on 3.35+).
    be.rollback_one_additive(&add, "operator")
        .await
        .expect("additive rollback via DROP COLUMN");

    let cols2 = be
        .actor()
        .query("PRAGMA main.table_info('people')")
        .await
        .expect("table_info");
    assert!(
        !cols2
            .iter()
            .any(|r| r.get(1).and_then(Clone::clone).as_deref() == Some("nickname")),
        "nickname column gone after DROP COLUMN rollback"
    );
}

// ---------------------------------------------------------------------------
// A `down` requiring a TYPE-change reversal returns the clear P3b-deferred typed
// error — NOT a silent skip, NOT a broken rebuild. Nothing is rolled back.
// ---------------------------------------------------------------------------
#[compio::test]
async fn rollback_rebuild_needed_returns_p3b_deferred_error() {
    let p = paths("rb_rebuild");
    let be = backend(&p);
    // The `up` is additive; the `down` is a type-change reversal that SQLite cannot
    // perform without the 12-step rebuild (P3b).
    let m = mig(
        "widen",
        "ALTER TABLE t ADD COLUMN amount INTEGER;",
        "ALTER TABLE t ALTER COLUMN amount TYPE TEXT;",
    );
    // Seed the base table + apply the migration so it is net-applied.
    let base = mig(
        "base",
        "CREATE TABLE t (id INTEGER PRIMARY KEY);",
        "DROP TABLE t;",
    );
    be.apply_one_additive(&base, "d").await.expect("apply base");
    be.apply_one_additive(&m, "d").await.expect("apply widen");

    let err = be
        .rollback_one_additive(&m, "operator")
        .await
        .expect_err("rebuild-needing down must be refused");
    match err {
        RollbackError::SqliteRebuildRequired { version, reason } => {
            assert_eq!(version, m.version.as_str());
            assert!(
                reason.contains("type or constraint change"),
                "reason names the rebuild cause: {reason}"
            );
        }
        other => panic!("expected SqliteRebuildRequired, got {other:?}"),
    }

    // Nothing was rolled back: no rolled_back event, the migration is still applied.
    let rb_rows = be
        .actor()
        .query(&format!(
            "SELECT version FROM \"_mig\".schema_migrations \
             WHERE event_kind='rolled_back' AND version='{}'",
            m.version.as_str()
        ))
        .await
        .expect("read rolled_back");
    assert!(
        rb_rows.is_empty(),
        "no rolled_back event for a refused rebuild"
    );
    let net = be.applied_sqlite().await.expect("net");
    assert!(
        net.iter()
            .any(|e| e.version == m.version.as_str() && e.phase == Phase::Completed),
        "migration stays net-applied after a refused rollback"
    );
}

// ---------------------------------------------------------------------------
// Confinement: a malicious `down` that tries to write `_mig` (under CreatorUp)
// is DENIED by the authorizer; the txn rolls back, no rolled_back event is
// written, and the migration stays applied.
// ---------------------------------------------------------------------------
#[compio::test]
async fn malicious_down_writing_mig_is_denied() {
    let p = paths("rb_mal");
    let be = backend(&p);
    let v_setup = mig(
        "setup",
        "CREATE TABLE t (id INTEGER PRIMARY KEY);",
        "DROP TABLE t;",
    );
    be.apply_one_additive(&v_setup, "d")
        .await
        .expect("apply setup");

    // A `down` whose statements are individually additive-looking but smuggle a
    // `_mig` write. The DROP TABLE classifies as additive (so we reach execution),
    // then the `_mig` DELETE is denied by the CreatorUp authorizer at prepare time.
    let mal = mig(
        "mal",
        "CREATE TABLE z (id INTEGER PRIMARY KEY);",
        "DROP TABLE z; DELETE FROM \"_mig\".schema_migrations;",
    );
    be.apply_one_additive(&mal, "d").await.expect("apply mal");

    let err = be
        .rollback_one_additive(&mal, "attacker")
        .await
        .expect_err("a _mig-writing down must be denied");
    // The denial surfaces as a Backend error wrapping the authorizer deny.
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("not authorized")
            || msg.to_ascii_lowercase().contains("authorization"),
        "expected an authorizer deny, got: {msg}"
    );

    // The journal is intact: schema_migrations still has the mal version completed,
    // and there is NO rolled_back event for it (the txn rolled back atomically).
    let net = be.applied_sqlite().await.expect("net");
    assert!(
        net.iter()
            .any(|e| e.version == mal.version.as_str() && e.phase == Phase::Completed),
        "mal migration stays applied after the denied rollback"
    );
    let rb = be
        .actor()
        .query(&format!(
            "SELECT version FROM \"_mig\".schema_migrations \
             WHERE event_kind='rolled_back' AND version='{}'",
            mal.version.as_str()
        ))
        .await
        .expect("read rolled_back");
    assert!(rb.is_empty(), "no rolled_back event from a denied rollback");
}

// ---------------------------------------------------------------------------
// The ORCHESTRATOR, end to end: two applied migrations, one `rollback` call,
// both `down`s run in reverse dependency order and both versions land in the
// outcome. Before this shipped, `plan_rollback` produced a plan nothing executed.
// ---------------------------------------------------------------------------
#[compio::test]
async fn rollback_unwinds_every_selected_migration_in_reverse_order() {
    let p = paths("rb_orchestrator");
    let be = backend(&p);
    let cfg =
        zero_migrate::ExecutorConfig::new("app_test", "app_test", support::no_inject("app_test"));

    let parent = mig(
        "create_parent",
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);",
        "DROP TABLE parent;",
    );
    let mut child = mig(
        "create_child",
        "CREATE TABLE child (id INTEGER PRIMARY KEY);",
        "DROP TABLE child;",
    );
    child.depends_on = vec![parent.version.clone()];
    let pv = parent.version.as_str().to_string();
    let cv = child.version.as_str().to_string();

    assert!(be
        .apply_one_additive(&parent, "d")
        .await
        .expect("apply parent"));
    assert!(be
        .apply_one_additive(&child, "d")
        .await
        .expect("apply child"));
    assert!(table_exists(&be, "parent").await);
    assert!(table_exists(&be, "child").await);

    let set = vec![parent.clone(), child.clone()];
    let outcome = zero_migrate::rollback(
        &be,
        &cfg,
        &zero_migrate::RollbackRequest::new(zero_migrate::RollbackTarget::All),
        &set,
        zero_migrate::Approval::Approved,
        "operator",
        &zero_migrate::SqliteDescriptorGuard::new(),
    )
    .await
    .expect("orchestrated rollback of both migrations");

    // The dependent is torn down BEFORE the thing it depends on.
    assert_eq!(
        outcome.rolled_back,
        vec![cv, pv],
        "child rolls back before parent (reverse depends_on order)"
    );
    assert!(outcome.skipped_irreversible.is_empty());
    assert!(!table_exists(&be, "child").await, "child table gone");
    assert!(!table_exists(&be, "parent").await, "parent table gone");
}

// ---------------------------------------------------------------------------
// The lock is REAL, not merely called. Hold the project lock, then ask for a
// rollback of nothing: without the acquire this returns a no-op `Ok`, so the
// refusal is what proves `rollback` takes the lock before it plans. SQLite
// refuses a same-instance re-acquire outright, which is what makes this arm
// single-process; PostgreSQL and MySQL would block on the server instead.
// ---------------------------------------------------------------------------
#[compio::test]
async fn rollback_refuses_while_another_holder_has_the_project_lock() {
    use zero_migrate::MigrationBackend;

    let p = paths("rb_lock_held");
    let be = backend(&p);
    let cfg =
        zero_migrate::ExecutorConfig::new("app_test", "app_test", support::no_inject("app_test"));

    be.acquire_project_lock(&cfg)
        .await
        .expect("take the project lock first");

    let err = zero_migrate::rollback(
        &be,
        &cfg,
        &zero_migrate::RollbackRequest::new(zero_migrate::RollbackTarget::All),
        &[],
        zero_migrate::Approval::Approved,
        "operator",
        &zero_migrate::SqliteDescriptorGuard::new(),
    )
    .await
    .expect_err("rollback must not proceed while the project lock is held");

    assert!(
        err.to_string().contains("already held"),
        "the refusal must name the held lock, not something downstream: {err}"
    );
}
