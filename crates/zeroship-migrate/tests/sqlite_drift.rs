//! Drift proofs for the SQLite migration backend (SQLite-parity design §2.7, P5
//! gate). Real temp-file SQLite throughout — `snapshot_schema` over the live app
//! file, checksum drift over the journal, sentinel recovery from sqlite_master.

use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::backend::MigrationBackend;
use zeroship_migrate::db::ExecutorConfig;
use zeroship_migrate::drift::diff_snapshots;
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

fn mig(name: &str, up: &str) -> Migration {
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

fn cfg() -> ExecutorConfig {
    ExecutorConfig::new("prj_test", "app")
}

// ---------------------------------------------------------------------------
// snapshot_schema reflects an applied CREATE TABLE: the table + its columns +
// PK constraint show up in the dialect-agnostic SchemaSnapshot.
// ---------------------------------------------------------------------------
#[compio::test]
async fn snapshot_reflects_applied_table() {
    let p = paths("snap1");
    let be = backend(&p);
    be.apply_one_additive(
        &mig(
            "create_users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER);",
        ),
        "deployer",
    )
    .await
    .expect("apply");

    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    let users = snap.tables.get("users").expect("users table in snapshot");
    let col_names: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(col_names, vec!["age", "email", "id"], "columns name-sorted");

    // email is NOT NULL; age is nullable.
    let email = users.columns.iter().find(|c| c.name == "email").unwrap();
    assert!(!email.nullable, "email NOT NULL");
    let age = users.columns.iter().find(|c| c.name == "age").unwrap();
    assert!(age.nullable, "age nullable");

    // The PRIMARY KEY surfaces as a constraint (matching the PG snapshot's
    // constraint bucket).
    assert!(
        users.constraints.iter().any(|c| c.kind == "PRIMARY KEY"),
        "PK constraint present: {:?}",
        users.constraints
    );

    // Internal/journal objects excluded: no `sqlite_*` or `_mig` table leaks in.
    assert!(
        snap.tables.keys().all(|k| !k.starts_with("sqlite_")),
        "no sqlite internal tables"
    );
    assert!(
        !snap.tables.contains_key("schema_migrations"),
        "journal objects must not appear in the app-schema snapshot"
    );
}

// ---------------------------------------------------------------------------
// A clean schema (snapshot vs itself) shows ZERO structural drift.
// ---------------------------------------------------------------------------
#[compio::test]
async fn clean_schema_zero_structural_drift() {
    let p = paths("snap_clean");
    let be = backend(&p);
    be.apply_one_additive(
        &mig("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);"),
        "d",
    )
    .await
    .expect("apply");

    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    // The snapshot diffed against itself is clean.
    let drift = diff_snapshots(&snap, &snap);
    assert!(drift.is_clean(), "self-diff must be clean: {drift:?}");
}

// ---------------------------------------------------------------------------
// Out-of-band ALTER is DETECTED: snapshot the schema, then ALTER the app file
// directly (a new column the migrations never declared), re-snapshot, and the
// structural diff surfaces the unexpected column.
// ---------------------------------------------------------------------------
#[compio::test]
async fn out_of_band_alter_detected_as_structural_drift() {
    let p = paths("snap_drift");
    let be = backend(&p);
    be.apply_one_additive(
        &mig("t", "CREATE TABLE widgets (id INTEGER PRIMARY KEY, label TEXT);"),
        "d",
    )
    .await
    .expect("apply");

    // The expected snapshot (what the migrations declared).
    let expected = be.snapshot_schema_sqlite().await.expect("expected snapshot");
    drop(be); // close the hardened connection so a raw conn can write the app file.

    // Out-of-band ALTER: add a column the migration journal knows nothing about,
    // via a PLAIN connection (simulating manual tampering of the app file).
    {
        let conn = rusqlite::Connection::open(&p.app).expect("reopen app file raw");
        conn.execute_batch("ALTER TABLE widgets ADD COLUMN sneaky TEXT;")
            .expect("out-of-band alter");
    }

    // Re-open the backend, re-snapshot the LIVE schema, and diff.
    let be2 = backend(&p);
    let actual = be2.snapshot_schema_sqlite().await.expect("actual snapshot");
    let drift = diff_snapshots(&expected, &actual);
    assert!(!drift.is_clean(), "drift must be detected");
    assert!(
        drift
            .unexpected_objects
            .iter()
            .any(|o| o.contains("sneaky")),
        "the out-of-band column must surface as unexpected: {drift:?}"
    );
}

// ---------------------------------------------------------------------------
// Checksum drift: a net-applied version whose journaled checksum no longer
// matches the supplied set's checksum is flagged (tamper / edited-after-apply).
// ---------------------------------------------------------------------------
#[compio::test]
async fn checksum_drift_detected_over_journal() {
    let p = paths("ck_drift");
    let be = backend(&p);
    let applied = mig("t", "CREATE TABLE t (id INTEGER PRIMARY KEY);");
    be.apply_one_additive(&applied, "d").await.expect("apply");

    // Clean: the SAME migration set ⇒ no drift.
    let clean = be
        .check_checksum_drift(&cfg(), std::slice::from_ref(&applied))
        .await
        .expect("drift read");
    assert!(clean.is_clean(), "matching set has no drift: {clean:?}");

    // Tamper: a migration with the SAME version but a DIFFERENT up ⇒ different
    // checksum ⇒ ChecksumDrift.
    let mut mutated = mig("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, extra TEXT);");
    mutated.version = applied.version.clone();
    let report = be
        .check_checksum_drift(&cfg(), std::slice::from_ref(&mutated))
        .await
        .expect("drift read");
    assert!(!report.is_clean(), "checksum drift must be detected");
    assert_eq!(report.checksum_drift.len(), 1);
    assert_eq!(report.checksum_drift[0].version, applied.version.as_str());
}

// ---------------------------------------------------------------------------
// Sentinel recovery: an applied table whose CREATE text carries an inline
// `/* __zsmask:... */` sentinel (the emitter's SQLite-side wire) is recovered
// into the snapshot's comment_sentinel — not silently dropped.
// ---------------------------------------------------------------------------
#[compio::test]
async fn mask_sentinel_recovered_from_sqlite_master() {
    let p = paths("snap_sentinel");
    let be = backend(&p);
    // The emitter writes the masked sibling column with an inline mask sentinel;
    // sqlite_master.sql preserves the comment verbatim. We hand-author the exact
    // shape the emitter produces (a `<col>_masked TEXT NOT NULL /* __zsmask:... */`).
    be.apply_one_additive(
        &mig(
            "with_mask",
            "CREATE TABLE accounts (\
                id INTEGER PRIMARY KEY, \
                ssn BLOB, \
                ssn_masked TEXT NOT NULL /* __zsmask:kind=last4,classification=pii */);",
        ),
        "d",
    )
    .await
    .expect("apply masked table");

    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    let accounts = snap.tables.get("accounts").expect("accounts table");
    let masked = accounts
        .columns
        .iter()
        .find(|c| c.name == "ssn_masked")
        .expect("masked sibling column present");
    assert_eq!(
        masked.comment_sentinel.as_deref(),
        Some("__zsmask:kind=last4,classification=pii"),
        "the inline mask sentinel must be recovered from sqlite_master.sql"
    );
    // A plain column carries no sentinel.
    let id = accounts.columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id.comment_sentinel, None);
}
