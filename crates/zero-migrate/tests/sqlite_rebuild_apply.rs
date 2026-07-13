//! The `SQLite` 12-step table REBUILD, end-to-end against REAL temp-file
//! `SQLite` through the hardened `SqliteBackend`. The faithful path:
//! the real `DeclarativeAuthor` builds the rebuild spec from a descriptor diff, and
//! the real backend executes the 12-step under confinement (`CreatorUp` rebuild DDL,
//! `EngineJournal` PRAGMA/check/journal, `foreign_keys` toggles straddling the txn).
//!
//! Coverage:
//! - type change with rows preserved + journaled + indexes recreated;
//! - nullability tighten (NULL → NOT NULL) and a column RENAME via rebuild;
//! - a goodie (mask/encrypted) survives the rebuild — the sentinel still recovers
//!   via the drift snapshot;
//! - FK integrity: a rebuild whose `foreign_key_check` would FAIL aborts with a
//!   typed error, the original table is intact, and `foreign_keys` is back ON;
//! - confinement during rebuild: a creator `up` cannot toggle `foreign_keys` nor
//!   reach `_mig`; after a rebuild the connection has `foreign_keys=ON`;
//! - failure path: an aborting rebuild leaves no wedge (the next apply succeeds) and
//!   `foreign_keys` is back ON.

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::{
    desired_snapshot, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor, IndexDescriptor,
    Migration, RebuildError, SchemaSnapshot, SqliteBackend, SqliteRebuild, SqliteRebuildSpec,
};
use zero_migrate::schema::query::SqlDialect;

const PROJECT: &str = "prj_demo";
const APP: &str = "app_demo";

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

fn sqlite_author() -> DeclarativeAuthor {
    DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Sqlite)
}

/// First-deploy live snapshot + ownership (the same `desired_snapshot` machinery the
/// engine uses; PG-spelled `data_type`, which the dialect-aware compare folds).
fn live_from(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_snapshot(PROJECT, descs).expect("first-deploy desired_snapshot");
    let ownership: HashMap<String, String> = d
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    (d.snapshot, ownership)
}

/// Apply every plain migration of a first-deploy plan through the real backend.
async fn apply_first_deploy(be: &SqliteBackend, desc: &[CollectionDescriptor]) {
    let desired = desired_snapshot(PROJECT, desc).expect("desired");
    let plan = sqlite_author()
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("first-deploy diff");
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("first-deploy apply {} must succeed: {e:?}", m.name));
    }
}

/// The single rebuild the second-deploy diff produces (asserts exactly one).
fn one_rebuild(v1: &[CollectionDescriptor], v2: &[CollectionDescriptor]) -> SqliteRebuild {
    let (live, ownership) = live_from(v1);
    let desired2 = desired_snapshot(PROJECT, v2).expect("v2 desired");
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, &[])
        .expect("second-deploy diff produces a rebuild plan");
    assert_eq!(plan.rebuilds.len(), 1, "expected exactly one rebuild");
    plan.rebuilds.into_iter().next().unwrap()
}

/// `PRAGMA foreign_keys` as an i64 (1 = ON, 0 = OFF), read in engine mode.
async fn foreign_keys_on(be: &SqliteBackend) -> bool {
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("engine mode");
    let rows = be
        .actor()
        .query("PRAGMA foreign_keys")
        .await
        .expect("read PRAGMA foreign_keys");
    rows.first()
        .and_then(|r| r.first())
        .and_then(|c| c.as_deref())
        .is_some_and(|s| s == "1")
}

// ---------------------------------------------------------------------------
// (1) Type change: t(n INTEGER) with rows → rebuild to n TEXT. Data preserved
//     (transformed to the new affinity), journaled, the table shape is the new
//     schema (PRAGMA), and the table's index is recreated.
// ---------------------------------------------------------------------------
#[compio::test]
async fn type_change_rebuild_preserves_data_and_recreates_index() {
    // v1: events(n: number → REAL affinity) + a user index over `n`.
    let v1 = vec![CollectionDescriptor {
        name: "events".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "n".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![IndexDescriptor {
            name: "events_n_idx".into(),
            columns: vec!["n".into()],
            unique: false,
        }],
        runtime_options: Default::default(),
    }];
    // v2: the same column re-typed to string → TEXT affinity. A genuine type change.
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("rebuild_type");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // Seed rows into the live table (engine mode lets the test write `main`).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.events (id, n) VALUES ('e1', 1), ('e2', 2), ('e3', 3)")
        .await
        .expect("seed rows");

    // The user index exists pre-rebuild.
    let idx_before = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='index' AND name='events_n_idx'")
        .await
        .expect("query index pre-rebuild");
    assert_eq!(idx_before.len(), 1, "events_n_idx exists before the rebuild");

    // The rebuild for the type change.
    let rb = one_rebuild(&v1, &v2);
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("rebuild applies");

    // Data is preserved (all three rows survived the copy).
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.events")
        .await
        .expect("count rows");
    assert_eq!(count[0][0].as_deref(), Some("3"), "all rows preserved");
    // The values carried across, TRANSFORMED to the new TEXT affinity. `n` was REAL
    // (number → double precision → REAL), so SQLite stored 1/2/3 as 1.0/2.0/3.0;
    // copying a REAL into a TEXT column stringifies it. The DATA is preserved (every
    // row, in order); its textual form reflects the new affinity — exactly the
    // "transformed/preserved" contract.
    let vals = be
        .actor()
        .query("SELECT n FROM main.events ORDER BY id")
        .await
        .expect("read n");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["1.0", "2.0", "3.0"],
        "values carried across the rebuild (transformed to the new TEXT affinity)"
    );

    // The table shape is the NEW schema: `n` now has TEXT affinity.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(events)")
        .await
        .expect("table_info");
    let n_type = info
        .iter()
        .find(|r| r[1].as_deref() == Some("n"))
        .and_then(|r| r[2].clone())
        .expect("n column present");
    assert_eq!(
        n_type.to_ascii_uppercase(),
        "TEXT",
        "the rebuilt column has the new TEXT type, got {n_type}"
    );

    // The user index was recreated after the swap.
    let idx_after = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='index' AND name='events_n_idx'")
        .await
        .expect("query index post-rebuild");
    assert_eq!(idx_after.len(), 1, "events_n_idx recreated after the rebuild");

    // The rebuild is journaled `completed`.
    let net = be.applied_sqlite().await.expect("read journal");
    let v = rb.migration.version.as_str();
    assert!(
        net.iter().any(|e| e.version == v
            && e.phase == zero_migrate::apply::journal::Phase::Completed),
        "the rebuild must be journaled completed"
    );

    // FK enforcement is restored to ON after the rebuild.
    assert!(foreign_keys_on(&be).await, "foreign_keys is ON post-rebuild");
}

// ---------------------------------------------------------------------------
// (2a) Nullability tighten: n NULL → NOT NULL, with the rows backfilled so the
//      NOT NULL holds. The rebuild applies; the new shape is NOT NULL.
// ---------------------------------------------------------------------------
#[compio::test]
async fn nullability_tighten_rebuild() {
    // v1: notes(body: optional string → nullable). v2: body required → NOT NULL.
    let v1 = vec![CollectionDescriptor {
        name: "notes".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "body".into(),
            ty: "string".into(),
            required: false,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].required = true;

    let p = paths("rebuild_nullability");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // Seed rows whose `body` is non-NULL (so the tightened NOT NULL holds after the
    // copy — a real backfilled migration). A NULL row would (correctly) trip the
    // new NOT NULL and abort; here we prove the happy path tightens cleanly.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.notes (id, body) VALUES ('n1', 'hello'), ('n2', 'world')")
        .await
        .expect("seed non-null rows");

    let rb = one_rebuild(&v1, &v2);
    assert!(
        rb.spec.reason.contains("nullability"),
        "the rebuild reason names the nullability change: {}",
        rb.spec.reason
    );
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("nullability-tighten rebuild applies");

    // The new shape is NOT NULL on `body`.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(notes)")
        .await
        .expect("table_info");
    let body_notnull = info
        .iter()
        .find(|r| r[1].as_deref() == Some("body"))
        .and_then(|r| r[3].clone()) // notnull column
        .expect("body present");
    assert_eq!(body_notnull, "1", "body is now NOT NULL");
    // Rows preserved.
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.notes")
        .await
        .expect("count");
    assert_eq!(count[0][0].as_deref(), Some("2"), "rows preserved");
}

// ---------------------------------------------------------------------------
// (2b) Column RENAME via rebuild: a hinted rename maps `to ← from`, carrying the
//      data into the renamed column.
// ---------------------------------------------------------------------------
#[compio::test]
async fn column_rename_rebuild_carries_data() {
    use zero_migrate::RenameHint;

    // v1: people(nickname). v2: people(handle). A hinted rename nickname → handle.
    let v1 = vec![CollectionDescriptor {
        name: "people".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "nickname".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].name = "handle".into();

    let p = paths("rebuild_rename");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.people (id, nickname) VALUES ('p1', 'ada'), ('p2', 'grace')")
        .await
        .expect("seed rows");

    // Build the rebuild via the differ WITH the rename hint.
    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2).expect("v2 desired");
    let hint = RenameHint {
        table: "people".into(),
        from: "nickname".into(),
        to: "handle".into(),
    };
    let plan = sqlite_author()
        .diff(&desired2, &live, &ownership, std::slice::from_ref(&hint))
        .expect("rename diff");
    assert_eq!(plan.rebuilds.len(), 1, "a rename yields one rebuild on SQLite");
    assert!(
        plan.renames.is_empty(),
        "the PG expand-contract rename path must NOT fire on SQLite"
    );
    let rb = &plan.rebuilds[0];
    assert!(
        rb.spec.reason.contains("rename column nickname → handle"),
        "the rebuild reason names the rename: {}",
        rb.spec.reason
    );
    // The copy mapping carries `handle ← nickname`.
    assert!(
        rb.spec
            .copy_columns
            .iter()
            .any(|(d, s)| d == "handle" && s == "nickname"),
        "the copy mapping must map handle ← nickname: {:?}",
        rb.spec.copy_columns
    );

    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("rename rebuild applies");

    // The data followed the rename: the renamed `handle` column holds the old values.
    let vals = be
        .actor()
        .query("SELECT handle FROM main.people ORDER BY id")
        .await
        .expect("read handle");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["ada", "grace"],
        "the renamed column carries the data"
    );
    // The old column name is gone.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(people)")
        .await
        .expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("nickname")),
        "the old column name is gone after the rename"
    );
}

// ---------------------------------------------------------------------------
// (3) A goodie survives the rebuild: a table with a mask + an encrypted column →
//     rebuild (an unrelated type change on a plain column) → the mask/enc sentinels
//     still recover via the drift snapshot (the new CREATE carries them inline).
// ---------------------------------------------------------------------------
#[compio::test]
async fn goodie_sentinels_survive_rebuild() {
    // v1: accounts(amount: number, ssn: masked, secret: encrypted).
    let v1 = vec![CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "amount".into(),
                ty: "number".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "ssn".into(),
                ty: "string".into(),
                mask: Some(serde_json::json!({ "kind": "last4", "classification": "pii" })),
                ..Default::default()
            },
            FieldDescriptor {
                name: "secret".into(),
                ty: "bytes".into(),
                encrypted: Some(serde_json::json!({ "mode": "randomized", "keyId": "k1" })),
                ..Default::default()
            },
        ],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    // v2: re-type `amount` number → string (a rebuild) — the goodie columns are
    // UNCHANGED but must survive the rebuild with their sentinels intact.
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("rebuild_goodie");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    let rb = one_rebuild(&v1, &v2);
    // The new CREATE carries the inline goodie sentinels (it came from the shared
    // emitter), so they survive the rebuild by construction.
    assert!(
        rb.spec.new_table_create.contains("/* zero-migrate:mask:")
            && rb.spec.new_table_create.contains("/* zero-migrate:enc:"),
        "the rebuilt CREATE must carry the inline mask + enc sentinels: {}",
        rb.spec.new_table_create
    );
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("goodie rebuild applies");

    // The drift snapshot recovers the sentinels from the rebuilt table's
    // sqlite_master.sql.
    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    let t = snap.tables.get("accounts").expect("accounts in snapshot");
    let secret = t.columns.iter().find(|c| c.name == "secret").expect("secret");
    assert!(
        secret
            .comment_sentinel
            .as_deref()
            .or(secret.encryption_sentinel.as_deref())
            .is_some_and(|s| s.contains("zero-migrate:enc:")),
        "encryption sentinel must survive the rebuild + recover via drift: {secret:?}"
    );
    let masked = t
        .columns
        .iter()
        .find(|c| c.name == "ssn_masked")
        .expect("ssn_masked sibling");
    assert!(
        masked
            .comment_sentinel
            .as_deref()
            .is_some_and(|s| s.contains("zero-migrate:mask:")),
        "mask sentinel must survive the rebuild + recover via drift: {masked:?}"
    );
}

// ---------------------------------------------------------------------------
// (4) FK integrity: a rebuild whose `foreign_key_check` would FAIL aborts with a
//     typed error, the ORIGINAL table is intact, and `foreign_keys` is back ON.
//
//     We construct this directly: a child table whose FK target rows are removed
//     out-of-band while FK enforcement is off, so the post-rebuild
//     `foreign_key_check` finds an orphan. The cleanest faithful construction:
//     seed a child row pointing at a parent id, then rebuild the CHILD with a spec
//     that copies the (now-orphan-making) FK column while the parent lacks the row.
// ---------------------------------------------------------------------------
#[compio::test]
async fn fk_violation_aborts_rebuild_intact_and_fk_back_on() {
    let p = paths("rebuild_fk_abort");
    let be = backend(&p);

    // Build a parent + child with an inline FK, via plain migrations (engine mode).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("BEGIN IMMEDIATE")
        .await
        .expect("begin");
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("creator");
    be.actor()
        .exec("CREATE TABLE parent (id TEXT PRIMARY KEY)")
        .await
        .expect("create parent");
    be.actor()
        .exec(
            "CREATE TABLE child (id TEXT PRIMARY KEY, parent_id TEXT \
             REFERENCES parent (id))",
        )
        .await
        .expect("create child");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("COMMIT").await.expect("commit");

    // Seed a parent row + a child referencing it (valid so far).
    be.actor()
        .exec("INSERT INTO main.parent (id) VALUES ('pa')")
        .await
        .expect("seed parent");
    be.actor()
        .exec("INSERT INTO main.child (id, parent_id) VALUES ('c1', 'pa')")
        .await
        .expect("seed child");
    // Now point the child at a NON-EXISTENT parent by disabling FK enforcement and
    // editing the row — the table is left referentially BROKEN, so any subsequent
    // rebuild's foreign_key_check must catch it. (foreign_keys toggling is engine-
    // only and outside a txn; restore it after.)
    be.actor()
        .exec("PRAGMA foreign_keys = OFF")
        .await
        .expect("fk off");
    be.actor()
        .exec("UPDATE main.child SET parent_id = 'ghost' WHERE id = 'c1'")
        .await
        .expect("orphan the child");
    be.actor()
        .exec("PRAGMA foreign_keys = ON")
        .await
        .expect("fk on");

    // A rebuild of `child` (same shape) that carries the orphaned row → the
    // foreign_key_check after the swap finds the dangling 'ghost' reference and
    // aborts. Construct the spec directly to keep the FK in the new table.
    let spec = SqliteRebuildSpec {
        table: "child".into(),
        tmp_table: SqliteRebuildSpec::tmp_name("child"),
        new_table_create: "CREATE TABLE \"child__zero_migrate_rebuild\" (\
            \"id\" TEXT PRIMARY KEY, \
            \"parent_id\" TEXT REFERENCES \"parent\" (\"id\"))"
            .into(),
        copy_columns: vec![
            ("id".into(), "id".into()),
            ("parent_id".into(), "parent_id".into()),
        ],
        recreate_objects: vec![],
        dropped_columns: vec![],
        reason: "fk integrity test rebuild".into(),
    };
    let m = rebuild_migration("child", &spec);

    let err = be
        .rebuild_one(&spec, &m, "deployer")
        .await
        .expect_err("the orphaned FK must abort the rebuild");
    match err {
        RebuildError::ForeignKeyViolation { table, violations } => {
            assert_eq!(table, "child");
            assert!(violations >= 1, "at least one violation row, got {violations}");
        }
        other => panic!("expected ForeignKeyViolation, got {other:?}"),
    }

    // The ORIGINAL `child` table is intact (the txn rolled back) — the orphaned row
    // is still there, and the temp table does NOT exist.
    let child_rows = be
        .actor()
        .query("SELECT parent_id FROM main.child WHERE id = 'c1'")
        .await
        .expect("read child");
    assert_eq!(
        child_rows[0][0].as_deref(),
        Some("ghost"),
        "the original child row is intact after the abort"
    );
    let tmp = be
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master WHERE type='table' \
             AND name='child__zero_migrate_rebuild'",
        )
        .await
        .expect("query tmp");
    assert!(tmp.is_empty(), "the temp table must be rolled back");

    // No journal row for the aborted rebuild.
    let net = be.applied_sqlite().await.expect("read journal");
    assert!(
        !net.iter().any(|e| e.version == m.version.as_str()),
        "an aborted rebuild leaves no journal row"
    );

    // foreign_keys is restored to ON in the abort path.
    assert!(
        foreign_keys_on(&be).await,
        "foreign_keys must be back ON after an FK-check abort"
    );

    // No wedge: a subsequent apply on the SAME backend succeeds.
    let next = simple_migration("after_abort", "CREATE TABLE after_abort (id TEXT PRIMARY KEY)");
    be.apply_one_additive(&next, "deployer")
        .await
        .expect("a subsequent apply must succeed (no wedge after the FK abort)");
}

// ---------------------------------------------------------------------------
// (5) Confinement during rebuild: a creator `up` CANNOT toggle PRAGMA foreign_keys
//     (still denied in CreatorUp) and cannot reach `_mig`; after a rebuild the
//     connection has foreign_keys=ON (asserted via PRAGMA).
// ---------------------------------------------------------------------------
#[compio::test]
async fn confinement_holds_across_rebuild() {
    let v1 = vec![CollectionDescriptor {
        name: "widgets".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "qty".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("rebuild_confine");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // A creator `up` cannot toggle foreign_keys (PRAGMA denied in CreatorUp).
    let pragma_attack = simple_migration("fk_toggle_attack", "PRAGMA foreign_keys = OFF;");
    let e = be
        .apply_one_additive(&pragma_attack, "attacker")
        .await
        .expect_err("a creator PRAGMA foreign_keys must be denied");
    assert!(
        e.is_authorizer_denied(),
        "the creator foreign_keys toggle must be an authorizer DENY, got {e}"
    );

    // A creator `up` cannot reach `_mig`.
    let mig_attack = simple_migration(
        "mig_write_attack",
        "INSERT INTO \"_mig\".schema_migrations (event_kind, version, name, checksum, \
         \"by\", phase, outcome, kind) VALUES ('applied', 'x', 'x', 'x', 'x', 'completed', \
         'success', 'apply');",
    );
    let e = be
        .apply_one_additive(&mig_attack, "attacker")
        .await
        .expect_err("a creator _mig write must be denied");
    assert!(
        e.is_authorizer_denied(),
        "the creator _mig write must be an authorizer DENY, got {e}"
    );

    // Now run a legitimate rebuild.
    let rb = one_rebuild(&v1, &v2);
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("rebuild applies");

    // After the rebuild the connection has foreign_keys=ON.
    assert!(
        foreign_keys_on(&be).await,
        "foreign_keys must be ON after the rebuild"
    );

    // And confinement still holds AFTER the rebuild: another creator PRAGMA is denied.
    let after = simple_migration("fk_toggle_after", "PRAGMA foreign_keys = OFF;");
    let e = be
        .apply_one_additive(&after, "attacker")
        .await
        .expect_err("a creator PRAGMA after the rebuild must still be denied");
    assert!(
        e.is_authorizer_denied(),
        "confinement must hold after the rebuild, got {e}"
    );
}

// ---------------------------------------------------------------------------
// (6) Failure path: an aborting rebuild (a malformed recreate-object statement)
//     leaves no wedge (the next apply succeeds) and foreign_keys=ON.
// ---------------------------------------------------------------------------
#[compio::test]
async fn aborting_rebuild_leaves_no_wedge_and_fk_on() {
    let p = paths("rebuild_wedge");
    let be = backend(&p);
    // Create a simple table to rebuild.
    let create = simple_migration("create_t", "CREATE TABLE t (id TEXT PRIMARY KEY, v TEXT)");
    be.apply_one_additive(&create, "deployer")
        .await
        .expect("create t");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.t (id, v) VALUES ('a', 'x')")
        .await
        .expect("seed");

    // A rebuild whose recreate step is a hard error (a CREATE INDEX over a
    // non-existent column) → the whole rebuild aborts mid-transaction.
    let spec = SqliteRebuildSpec {
        table: "t".into(),
        tmp_table: SqliteRebuildSpec::tmp_name("t"),
        new_table_create: "CREATE TABLE \"t__zero_migrate_rebuild\" (\"id\" TEXT PRIMARY KEY, \"v\" TEXT)"
            .into(),
        copy_columns: vec![("id".into(), "id".into()), ("v".into(), "v".into())],
        // A bogus index over a column that does not exist → CREATE INDEX errors.
        recreate_objects: vec![
            "CREATE INDEX \"t_bogus_idx\" ON \"t\" (\"does_not_exist\")".into(),
        ],
        dropped_columns: vec![],
        reason: "wedge test".into(),
    };
    let m = rebuild_migration("t", &spec);
    let res = be.rebuild_one(&spec, &m, "deployer").await;
    assert!(res.is_err(), "the bogus recreate must abort the rebuild");

    // The original table + its row are intact (rolled back), the temp table is gone.
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.t")
        .await
        .expect("count");
    assert_eq!(count[0][0].as_deref(), Some("1"), "original row intact");
    let tmp = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='t__zero_migrate_rebuild'")
        .await
        .expect("query tmp");
    assert!(tmp.is_empty(), "temp table rolled back");

    // foreign_keys is back ON.
    assert!(foreign_keys_on(&be).await, "foreign_keys ON after the abort");

    // No wedge: the connection is back in autocommit and a fresh apply succeeds.
    assert!(
        be.actor().is_autocommit().await.expect("probe"),
        "connection back in autocommit after the aborted rebuild"
    );
    let next = simple_migration("after_wedge", "CREATE TABLE after_wedge (id TEXT PRIMARY KEY)");
    be.apply_one_additive(&next, "deployer")
        .await
        .expect("a subsequent apply must succeed (no wedge)");
}

// ---------------------------------------------------------------------------
// A creator TRIGGER and a PARTIAL index survive a rebuild. The lossy pre-fix
//      path rebuilt indexes from the DESIRED IndexSnapshot (dropping the WHERE
//      clause) and NEVER recreated triggers (DROP TABLE silently destroyed them).
//      The fix captures every dependent object's `sql` VERBATIM from the live
//      sqlite_master before the drop and replays it after the rename. Asserts: the
//      trigger STILL EXISTS and FIRES post-rebuild, and the partial index survives
//      with its WHERE clause intact. RED pre-fix (trigger gone; index loses WHERE).
// ---------------------------------------------------------------------------
#[compio::test]
async fn creator_trigger_and_partial_index_survive_rebuild() {
    // v1: items(n: number) + a sink table the trigger writes to. v2: n re-typed to
    // string → a genuine type-change rebuild.
    let v1 = vec![CollectionDescriptor {
        name: "items".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "n".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("rebuild_trigger_partial");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // Add — directly, as a creator would in a prior CreatorUp migration — a sink
    // table, an AFTER INSERT trigger on `items` that writes the sink, and a PARTIAL
    // index on `items` (a WHERE clause the lossy desired-snapshot recreate dropped).
    // We do this in one engine-wrapped txn flipping to CreatorUp for the creator DDL.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("BEGIN IMMEDIATE").await.expect("begin");
    be.actor().set_mode(Mode::CreatorUp).await.expect("creator");
    be.actor()
        .exec("CREATE TABLE trg_sink (id TEXT PRIMARY KEY, note TEXT)")
        .await
        .expect("create sink");
    be.actor()
        .exec(
            "CREATE TRIGGER items_ai AFTER INSERT ON items \
             BEGIN INSERT INTO trg_sink (id, note) VALUES (NEW.id, 'fired'); END",
        )
        .await
        .expect("create trigger");
    be.actor()
        .exec("CREATE INDEX items_partial_idx ON items (n) WHERE n > 10")
        .await
        .expect("create partial index");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("COMMIT").await.expect("commit");

    // Seed a baseline row (engine mode).
    be.actor()
        .exec("INSERT INTO main.items (id, n) VALUES ('i0', 1)")
        .await
        .expect("seed");

    // The rebuild (type change number → string).
    let rb = one_rebuild(&v1, &v2);
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("rebuild applies");

    // The TRIGGER still exists post-rebuild.
    let trg = be
        .actor()
        .query(
            "SELECT name FROM main.sqlite_master WHERE type='trigger' AND name='items_ai'",
        )
        .await
        .expect("query trigger");
    assert_eq!(
        trg.len(),
        1,
        "the creator trigger must survive the rebuild (pre-fix: silently destroyed)"
    );

    // And it FIRES: insert a fresh row and assert the sink got the side-effect.
    be.actor()
        .exec("INSERT INTO main.items (id, n) VALUES ('i1', 5)")
        .await
        .expect("insert post-rebuild");
    let sink = be
        .actor()
        .query("SELECT note FROM main.trg_sink WHERE id='i1'")
        .await
        .expect("read sink");
    assert_eq!(
        sink.first().and_then(|r| r.first()).and_then(|c| c.as_deref()),
        Some("fired"),
        "the trigger must FIRE post-rebuild (its side-effect lands in the sink)"
    );

    // The PARTIAL index survives WITH its WHERE clause. Read the verbatim sql.
    let idx = be
        .actor()
        .query(
            "SELECT sql FROM main.sqlite_master WHERE type='index' AND name='items_partial_idx'",
        )
        .await
        .expect("query partial index");
    let idx_sql = idx
        .first()
        .and_then(|r| r.first())
        .and_then(std::clone::Clone::clone)
        .expect("partial index must still exist post-rebuild");
    assert!(
        idx_sql.to_ascii_uppercase().contains("WHERE"),
        "the partial index must survive WITH its WHERE clause (pre-fix: lossy recreate \
         dropped it); got sql = {idx_sql}"
    );
}

// ---------------------------------------------------------------------------
// (C2b) A dependent object that genuinely cannot be replayed (it references a column
//       the new shape DROPS) FAILS CLOSED with the typed DependentReplayFailed error
//       — never silently lost. The original table + dependent are intact. We use an
//       INDEX over a column the new shape omits: SQLite validates an index's columns
//       at CREATE time, so the verbatim replay onto the new (column-less) table errors
//       → the executor aborts with DependentReplayFailed rather than dropping it.
//       (A trigger's column refs resolve lazily in SQLite, so an index is the faithful
//       vehicle for "captured DDL that cannot re-apply".)
// ---------------------------------------------------------------------------
#[compio::test]
async fn dependent_referencing_dropped_column_fails_closed() {
    let p = paths("rebuild_dep_fail");
    let be = backend(&p);

    // Create t(id, drop_me) + an index over `drop_me`, in one txn.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("BEGIN IMMEDIATE").await.expect("begin");
    be.actor().set_mode(Mode::CreatorUp).await.expect("creator");
    be.actor()
        .exec("CREATE TABLE t (id TEXT PRIMARY KEY, drop_me TEXT)")
        .await
        .expect("create t");
    be.actor()
        .exec("CREATE INDEX t_on_drop_me ON t (drop_me)")
        .await
        .expect("create index referencing drop_me");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("COMMIT").await.expect("commit");

    // A rebuild whose new shape DROPS `drop_me` (only `id` survives). The captured
    // index is over `drop_me`, which the new table lacks → verbatim replay must fail.
    let spec = SqliteRebuildSpec {
        table: "t".into(),
        tmp_table: SqliteRebuildSpec::tmp_name("t"),
        new_table_create: "CREATE TABLE \"t__zero_migrate_rebuild\" (\"id\" TEXT PRIMARY KEY)".into(),
        copy_columns: vec![("id".into(), "id".into())],
        recreate_objects: vec![],
        // EMPTY on purpose: a DIRECT-spec caller that does not declare the dropped
        // column gets the FAIL-CLOSED safety net (the captured index over `drop_me`
        // cannot replay and aborts). The declarative path, by contrast, populates
        // `dropped_columns` so the dependent is skipped — see the
        // `h1_drop_column_in_index_routes_to_rebuild` faithful test.
        dropped_columns: vec![],
        reason: "drop column drop_me".into(),
    };
    let m = rebuild_migration("t", &spec);

    let err = be
        .rebuild_one(&spec, &m, "deployer")
        .await
        .expect_err("a dependent referencing a dropped column must FAIL CLOSED");
    match err {
        RebuildError::DependentReplayFailed {
            table, kind, object, ..
        } => {
            assert_eq!(table, "t");
            assert_eq!(kind, "index");
            assert_eq!(object, "t_on_drop_me");
        }
        other => panic!("expected DependentReplayFailed, got {other:?}"),
    }

    // The ORIGINAL table is intact (txn rolled back): `drop_me` is still a column.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(t)")
        .await
        .expect("table_info");
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("drop_me")),
        "the original table must be intact after the fail-closed abort"
    );
    // The index is still there.
    let idx = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='index' AND name='t_on_drop_me'")
        .await
        .expect("query index");
    assert_eq!(idx.len(), 1, "the dependent index must be intact (never lost)");
    // foreign_keys back ON.
    assert!(foreign_keys_on(&be).await, "foreign_keys ON after the fail-closed abort");
}

// ---------------------------------------------------------------------------
// A cross-table FK orphan is caught by the UNSCOPED foreign_key_check. We
//      rebuild a PARENT table dropping a row a CHILD references. A check scoped to
//      the parent passes (the orphan is in the CHILD); the unscoped check catches it.
//      Asserts: typed ForeignKeyViolation abort, both tables intact, FK back ON.
//      RED pre-fix (the scoped `foreign_key_check(parent)` passed and committed).
// ---------------------------------------------------------------------------
#[compio::test]
async fn cross_table_fk_orphan_caught_by_unscoped_check() {
    let p = paths("rebuild_xtable_fk");
    let be = backend(&p);

    // parent(id) + child(id, parent_id REFERENCES parent), via plain DDL.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("BEGIN IMMEDIATE").await.expect("begin");
    be.actor().set_mode(Mode::CreatorUp).await.expect("creator");
    be.actor()
        .exec("CREATE TABLE parent (id TEXT PRIMARY KEY, label TEXT)")
        .await
        .expect("create parent");
    be.actor()
        .exec("CREATE TABLE child (id TEXT PRIMARY KEY, parent_id TEXT REFERENCES parent (id))")
        .await
        .expect("create child");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("COMMIT").await.expect("commit");

    // Seed parent 'pa' and a child referencing it (referentially valid).
    be.actor()
        .exec("INSERT INTO main.parent (id, label) VALUES ('pa', 'keep')")
        .await
        .expect("seed parent");
    be.actor()
        .exec("INSERT INTO main.child (id, parent_id) VALUES ('c1', 'pa')")
        .await
        .expect("seed child");

    // Rebuild the PARENT but DROP the 'pa' row (copy only rows where id != 'pa').
    // This orphans child.c1, whose parent_id='pa' no longer resolves. A
    // foreign_key_check scoped to `parent` sees no orphan IN parent and passes; only
    // the UNSCOPED check (which also visits `child`) catches the orphan.
    let spec = SqliteRebuildSpec {
        table: "parent".into(),
        tmp_table: SqliteRebuildSpec::tmp_name("parent"),
        new_table_create: "CREATE TABLE \"parent__zero_migrate_rebuild\" (\
            \"id\" TEXT PRIMARY KEY, \"label\" TEXT)"
            .into(),
        // copy_columns empty would copy nothing; we need a filtered copy, so we
        // instead pre-delete the row out-of-band below and copy straight across.
        copy_columns: vec![("id".into(), "id".into()), ("label".into(), "label".into())],
        recreate_objects: vec![],
        dropped_columns: vec![],
        reason: "parent rebuild dropping referenced row".into(),
    };
    // Drop the referenced parent row BEFORE the rebuild (engine mode, FK is ON so we
    // must delete the child-less way: temporarily the child still points at it, so
    // deleting 'pa' with FK ON would fail; do it under the rebuild's own FK-OFF window
    // by instead removing it inside the copy). Simplest faithful path: delete 'pa'
    // with FK enforcement off, mirroring how the rebuild's FK-OFF window would let a
    // parent row vanish, then let the rebuild copy the (now smaller) parent set.
    be.actor()
        .exec("PRAGMA foreign_keys = OFF")
        .await
        .expect("fk off");
    be.actor()
        .exec("DELETE FROM main.parent WHERE id = 'pa'")
        .await
        .expect("drop referenced parent row");
    be.actor()
        .exec("PRAGMA foreign_keys = ON")
        .await
        .expect("fk on");

    let m = rebuild_migration("parent", &spec);
    let err = be
        .rebuild_one(&spec, &m, "deployer")
        .await
        .expect_err("the cross-table orphan must abort the rebuild (unscoped FK check)");
    match err {
        RebuildError::ForeignKeyViolation { table, violations } => {
            assert_eq!(table, "parent");
            assert!(violations >= 1, "at least one orphan, got {violations}");
        }
        other => panic!("expected ForeignKeyViolation, got {other:?}"),
    }

    // Both tables intact (txn rolled back): the child orphan row is still present.
    let child = be
        .actor()
        .query("SELECT parent_id FROM main.child WHERE id='c1'")
        .await
        .expect("read child");
    assert_eq!(
        child.first().and_then(|r| r.first()).and_then(|c| c.as_deref()),
        Some("pa"),
        "the child row is intact after the abort"
    );
    let tmp = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='parent__zero_migrate_rebuild'")
        .await
        .expect("query tmp");
    assert!(tmp.is_empty(), "the temp table must be rolled back");
    assert!(foreign_keys_on(&be).await, "foreign_keys ON after the cross-table abort");
}

// ---------------------------------------------------------------------------
// A creator-pre-created `<t>__zero_migrate_rebuild` temp table does NOT pollute the
//      rebuild. Pre-fix the shared CREATE's `IF NOT EXISTS` silently REUSED the
//      stale temp (junk column + junk row), then RENAMEd the pollution into place.
//      The fix DROPs the stale temp first. Asserts: post-rebuild `t` has the NEW
//      shape (no junk column) and the junk row is gone. RED pre-fix.
// ---------------------------------------------------------------------------
#[compio::test]
async fn pre_created_temp_table_does_not_pollute_rebuild() {
    // v1: gadgets(n: number). v2: n → string (type-change rebuild).
    let v1 = vec![CollectionDescriptor {
        name: "gadgets".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "n".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("rebuild_temp_collision");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // Seed a legit row.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.gadgets (id, n) VALUES ('g1', 1)")
        .await
        .expect("seed");

    // A creator pre-creates the engine's temp name with JUNK shape + a JUNK row, as a
    // prior CreatorUp migration would. (One txn flipping to CreatorUp for the DDL.)
    let tmp_name = SqliteRebuildSpec::tmp_name("gadgets");
    assert_eq!(tmp_name, "gadgets__zero_migrate_rebuild");
    be.actor().exec("BEGIN IMMEDIATE").await.expect("begin");
    be.actor().set_mode(Mode::CreatorUp).await.expect("creator");
    be.actor()
        .exec("CREATE TABLE \"gadgets__zero_migrate_rebuild\" (junk TEXT)")
        .await
        .expect("pre-create junk temp");
    be.actor()
        .exec("INSERT INTO \"gadgets__zero_migrate_rebuild\" (junk) VALUES ('POLLUTION')")
        .await
        .expect("seed junk");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor().exec("COMMIT").await.expect("commit");

    // The rebuild must NOT reuse the polluted temp.
    let rb = one_rebuild(&v1, &v2);
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("rebuild applies cleanly despite the pre-created temp");

    // Post-rebuild `gadgets` has the NEW shape — a `junk` column would prove the
    // polluted temp was reused.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(gadgets)")
        .await
        .expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("junk")),
        "the rebuilt table must NOT carry the creator-pre-created junk column \
         (pre-fix: IF NOT EXISTS reused the polluted temp)"
    );
    // And `n` is present with TEXT affinity (the real new shape).
    let n_type = info
        .iter()
        .find(|r| r[1].as_deref() == Some("n"))
        .and_then(|r| r[2].clone())
        .expect("n present");
    assert_eq!(n_type.to_ascii_uppercase(), "TEXT", "n re-typed to TEXT");
    // The junk row never made it in: the original legit row is the only one.
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.gadgets")
        .await
        .expect("count");
    assert_eq!(
        count[0][0].as_deref(),
        Some("1"),
        "only the legit row survives; the POLLUTION junk row never entered"
    );
}

// ---------------------------------------------------------------------------
// Dropping a CONSTRAINED column routes to the 12-step rebuild (a native
// SQLite `ALTER TABLE … DROP COLUMN` ERRORS on such a column, aborting the
// migration). Two faithful end-to-end cases on the REAL hardened backend.
// ---------------------------------------------------------------------------

// DROP a column that participates in an INDEX. The detector source (1)
// (IndexSnapshot::columns) catches it. The drop must apply via REBUILD: the
// column is gone, its index is gone, a SURVIVING column's index is intact, and
// the other column's data is preserved.
#[compio::test]
async fn h1_drop_column_in_index_routes_to_rebuild() {
    // v1: events(n number [indexed], extra string [indexed]).
    let v1 = vec![CollectionDescriptor {
        name: "events".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "n".into(),
                ty: "number".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "extra".into(),
                ty: "string".into(),
                required: false,
                ..Default::default()
            },
        ],
        indexes: vec![
            IndexDescriptor {
                name: "events_n_idx".into(),
                columns: vec!["n".into()],
                unique: false,
            },
            IndexDescriptor {
                name: "events_extra_idx".into(),
                columns: vec!["extra".into()],
                unique: false,
            },
        ],
        runtime_options: Default::default(),
    }];
    // v2: drop `extra` (and its index). `n` + its index survive.
    let v2 = vec![CollectionDescriptor {
        name: "events".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "n".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![IndexDescriptor {
            name: "events_n_idx".into(),
            columns: vec!["n".into()],
            unique: false,
        }],
        runtime_options: Default::default(),
    }];

    let p = paths("h1_idx");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // Seed rows.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.events (id, n, extra) VALUES ('e1', 1, 'a'), ('e2', 2, 'b')")
        .await
        .expect("seed rows");

    // The second-deploy diff MUST produce a rebuild (not a native DROP COLUMN).
    let rb = one_rebuild(&v1, &v2);
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("the constrained-column drop must apply via rebuild");

    // The dropped column is gone.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(events)")
        .await
        .expect("table_info");
    assert!(
        !info.iter().any(|r| r[1].as_deref() == Some("extra")),
        "the dropped column `extra` must be gone after the rebuild"
    );
    // Its index is gone; the surviving column's index is intact. (Other indexes are
    // the platform system-field indexes emitted inline by the create; we assert the
    // two user indexes specifically.)
    let idx = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='index'")
        .await
        .expect("index query");
    let idx_names: Vec<String> = idx.iter().filter_map(|r| r[0].clone()).collect();
    assert!(
        !idx_names.iter().any(|n| n == "events_extra_idx"),
        "events_extra_idx (over the dropped column) must be gone: {idx_names:?}"
    );
    assert!(
        idx_names.iter().any(|n| n == "events_n_idx"),
        "events_n_idx (over the surviving column) must be intact: {idx_names:?}"
    );
    // Data preserved (both rows, `n` intact).
    let vals = be
        .actor()
        .query("SELECT n FROM main.events ORDER BY id")
        .await
        .expect("read n");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["1", "2"],
        "surviving column data preserved across the drop-via-rebuild"
    );
    assert!(foreign_keys_on(&be).await, "foreign_keys ON post-rebuild");
}

// DROP a column that participates in a CHECK constraint (emitted inline by
// a numeric `min`). The CHECK is NOT in the structured SQLite snapshot — the
// detector's source (3) (stored CREATE text scan) catches it. The drop must apply
// via REBUILD: the column is gone and the other column's data is preserved.
#[compio::test]
async fn h1_drop_column_in_check_routes_to_rebuild() {
    // v1: scores(label string, points number [min:0 → inline CHECK (points >= 0)]).
    let v1 = vec![CollectionDescriptor {
        name: "scores".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "label".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "points".into(),
                ty: "number".into(),
                required: true,
                min: Some(0.0),
                ..Default::default()
            },
        ],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    // v2: drop `points` (the CHECK-constrained column). `label` survives.
    let v2 = vec![CollectionDescriptor {
        name: "scores".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "label".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];

    let p = paths("h1_check");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // Sanity: the live table's stored DDL really carries the CHECK over `points`
    // (so this test exercises the source-(3) path, not source (1)/(2)).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let ddl = be
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE type='table' AND name='scores'")
        .await
        .expect("read stored DDL");
    let ddl_text = ddl[0][0].clone().unwrap_or_default();
    assert!(
        ddl_text.to_ascii_uppercase().contains("CHECK") && ddl_text.contains("points"),
        "the live DDL must carry a CHECK referencing points: {ddl_text}"
    );

    be.actor()
        .exec("INSERT INTO main.scores (id, label, points) VALUES ('s1', 'x', 5), ('s2', 'y', 9)")
        .await
        .expect("seed rows");

    // The second-deploy diff MUST produce a rebuild (a native DROP COLUMN would
    // ERROR: the column is used by a CHECK constraint).
    let rb = one_rebuild(&v1, &v2);
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("the CHECK-constrained-column drop must apply via rebuild");

    // The dropped column is gone; the CHECK referencing it is gone with it.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(scores)")
        .await
        .expect("table_info");
    assert!(
        !info.iter().any(|r| r[1].as_deref() == Some("points")),
        "the dropped CHECK column `points` must be gone after the rebuild"
    );
    let ddl_after = be
        .actor()
        .query("SELECT sql FROM main.sqlite_master WHERE type='table' AND name='scores'")
        .await
        .expect("read stored DDL after");
    let after_text = ddl_after[0][0].clone().unwrap_or_default();
    assert!(
        !after_text.contains("points"),
        "the surviving table DDL must not reference the dropped column: {after_text}"
    );
    // Surviving column data preserved.
    let labels = be
        .actor()
        .query("SELECT label FROM main.scores ORDER BY id")
        .await
        .expect("read label");
    assert_eq!(
        labels.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["x".to_string(), "y".to_string()],
        "surviving column data preserved across the CHECK-drop-via-rebuild"
    );
    assert!(foreign_keys_on(&be).await, "foreign_keys ON post-rebuild");
}

// ---------------------------------------------------------------------------
// (NULL-LOOSEN) Nullability LOOSEN (NOT NULL → nullable) via rebuild. Only the
// TIGHTEN direction was tested (`nullability_tighten_rebuild`); this is the
// loosen half. v1: body required (NOT NULL); v2: body optional (nullable). The
// second-deploy diff yields one rebuild ("alter column body nullability false →
// true"); applying it preserves the rows and the new shape is nullable.
// ---------------------------------------------------------------------------
#[compio::test]
async fn nullability_loosen_rebuild_preserves_data() {
    // v1: notes(body: required string → NOT NULL). v2: body optional → nullable.
    let v1 = vec![CollectionDescriptor {
        name: "notes".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "body".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].required = false;

    let p = paths("rebuild_null_loosen");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // The pre-rebuild shape is NOT NULL on `body`.
    let info_before = be
        .actor()
        .query("PRAGMA main.table_info(notes)")
        .await
        .expect("table_info pre");
    let notnull_before = info_before
        .iter()
        .find(|r| r[1].as_deref() == Some("body"))
        .and_then(|r| r[3].clone())
        .expect("body present");
    assert_eq!(notnull_before, "1", "body starts NOT NULL");

    // Seed rows (engine mode lets the test write `main`).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.notes (id, body) VALUES ('n1', 'hello'), ('n2', 'world')")
        .await
        .expect("seed rows");

    let rb = one_rebuild(&v1, &v2);
    assert!(
        rb.spec.reason.contains("nullability"),
        "the rebuild reason names the nullability change: {}",
        rb.spec.reason
    );
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("nullability-loosen rebuild applies");

    // The new shape is NULLABLE on `body` (notnull = 0).
    let info_after = be
        .actor()
        .query("PRAGMA main.table_info(notes)")
        .await
        .expect("table_info post");
    let notnull_after = info_after
        .iter()
        .find(|r| r[1].as_deref() == Some("body"))
        .and_then(|r| r[3].clone())
        .expect("body present");
    assert_eq!(notnull_after, "0", "body is now nullable after the loosen rebuild");

    // The data is preserved across the rebuild.
    let vals = be
        .actor()
        .query("SELECT body FROM main.notes ORDER BY id")
        .await
        .expect("read body");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["hello", "world"],
        "rows preserved across the nullability-loosen rebuild"
    );
    // A nullable column now ACCEPTS a NULL insert (the loosen actually took effect).
    be.actor()
        .exec("INSERT INTO main.notes (id, body) VALUES ('n3', NULL)")
        .await
        .expect("a NULL body is now accepted after the loosen");
    assert!(foreign_keys_on(&be).await, "foreign_keys ON post-rebuild");
}

// ---------------------------------------------------------------------------
// (DROP-FK) Removing a FOREIGN KEY via rebuild → positive end-state: the FK is
// GONE (`PRAGMA foreign_key_list` is empty) and integrity holds. Previously only
// the deferred-FK-is-an-error path existed; this proves the reachable drop. v1:
// posts(author ref→users); v2: posts(author plain string, no FK). The diff yields
// one rebuild ("drop foreign key posts.author_fkey").
// ---------------------------------------------------------------------------
#[compio::test]
async fn drop_foreign_key_via_rebuild_removes_the_fk() {
    let users = CollectionDescriptor {
        name: "users".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "handle".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    };
    let posts_fk = CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("users".into()),
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    };
    // v2: `author` becomes a plain string column — the FK is dropped.
    let posts_nofk = CollectionDescriptor {
        name: "posts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "string".into(),
            required: false,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    };
    let v1 = vec![users.clone(), posts_fk];
    let v2 = vec![users, posts_nofk];

    let p = paths("rebuild_drop_fk");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

    // Pre-rebuild: `posts` has exactly one FK (author → users).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let fk_before = be
        .actor()
        .query("PRAGMA main.foreign_key_list(posts)")
        .await
        .expect("fk_list pre");
    assert_eq!(fk_before.len(), 1, "posts starts with one FK: {fk_before:?}");

    // Seed a referentially-valid pair so integrity is intact going in.
    be.actor()
        .exec("INSERT INTO main.users (id, handle) VALUES ('u1', 'ada')")
        .await
        .expect("seed user");
    be.actor()
        .exec("INSERT INTO main.posts (id, author) VALUES ('p1', 'u1')")
        .await
        .expect("seed post");

    // The drop-FK diff yields exactly one rebuild on `posts`.
    let rb = one_rebuild(&v1, &v2);
    assert_eq!(rb.spec.table, "posts");
    assert!(
        rb.spec.reason.contains("drop foreign key"),
        "the rebuild reason names the FK drop: {}",
        rb.spec.reason
    );
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("drop-FK rebuild applies");

    // POSITIVE end-state: `posts` now has NO foreign keys.
    let fk_after = be
        .actor()
        .query("PRAGMA main.foreign_key_list(posts)")
        .await
        .expect("fk_list post");
    assert!(
        fk_after.is_empty(),
        "the FK must be GONE after the drop-FK rebuild: {fk_after:?}"
    );
    // Integrity holds, and the (formerly FK-constrained) column is now free: a row
    // whose `author` does NOT exist in `users` is now insertable (the FK is gone).
    be.actor()
        .exec("INSERT INTO main.posts (id, author) VALUES ('p2', 'no_such_user')")
        .await
        .expect("a dangling author is now allowed (no FK)");
    // The original row survived the rebuild.
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.posts")
        .await
        .expect("count");
    assert_eq!(count[0][0].as_deref(), Some("2"), "rows preserved + new row inserted");
    // A whole-DB integrity check passes (no orphaned constraints left behind).
    let integrity = be
        .actor()
        .query("PRAGMA main.foreign_key_check")
        .await
        .expect("fk check");
    assert!(integrity.is_empty(), "foreign_key_check is clean: {integrity:?}");
    assert!(foreign_keys_on(&be).await, "foreign_keys ON post-rebuild");
}

// ---------------------------------------------------------------------------
// Helpers: build a Migration without the declarative author (for direct specs).
// ---------------------------------------------------------------------------

fn simple_migration(name: &str, up: &str) -> Migration {
    use zero_migrate::model::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: APP,
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
        owner_app: APP.to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

// ---------------------------------------------------------------------------
// REGRESSION: the executor seam `MigrationBackend::rebuild_one`
// enforces the PER-VERSION approval SCOPE as a TRUE second gate.
//
// Before the fix, the trait `rebuild_one` took NO `scope` — so a direct seam
// caller driving `MigrationBackend::rebuild_one(be, .., Approval::Approved-equiv)`
// could rebuild a destructive table the operator never individually reviewed,
// bypassing the engine's per-version scope gate. This test drives the SEAM
// DIRECTLY with an EMPTY `Versions` scope (admits nothing) and asserts
// `ApprovalNotScoped` + that the live table is UNCHANGED (no rebuild ran). It
// fails RED on the pre-fix seam (no scope param ⇒ the rebuild ran).
// ---------------------------------------------------------------------------
#[compio::test]
async fn rebuild_one_seam_refuses_destructive_rebuild_outside_version_scope() {
    use zero_migrate::{ApplyError, ApprovalScope, MigrationBackend};

    // A genuine type-change rebuild (destructive=true by construction).
    let v1 = vec![CollectionDescriptor {
        name: "events".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "n".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("rebuild_scope");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.events (id, n) VALUES ('e1', 1)")
        .await
        .expect("seed row");

    let rb = one_rebuild(&v1, &v2);
    assert!(
        rb.migration.flags.destructive,
        "a SQLite rebuild migration is destructive by construction"
    );

    // Drive the TRAIT seam DIRECTLY with an EMPTY Versions scope (admits NO
    // version). The executor-layer scope gate must refuse the destructive rebuild.
    let refused = MigrationBackend::rebuild_one(
        &be,
        &rb.spec,
        &rb.migration,
        &ApprovalScope::Versions(std::collections::BTreeSet::new()),
        "deployer",
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(ApplyError::ApprovalNotScoped { ref version })
                if version == rb.migration.version.as_str()
        ),
        "the rebuild_one seam must refuse a destructive rebuild whose version is \
         outside the approved scope (executor-layer per-version gate), got {refused:?}"
    );

    // NOTHING rebuilt: the live `n` column still has its ORIGINAL (REAL) affinity,
    // and the rebuild version is NOT journaled.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(events)")
        .await
        .expect("table_info");
    let n_type = info
        .iter()
        .find(|r| r[1].as_deref() == Some("n"))
        .and_then(|r| r[2].clone())
        .expect("n column present");
    assert_eq!(
        n_type.to_ascii_uppercase(),
        "REAL",
        "a scope-refused rebuild leaves the original column type intact, got {n_type}"
    );
    let net = be.applied_sqlite().await.expect("read journal");
    assert!(
        !net.iter().any(|e| e.version == rb.migration.version.as_str()),
        "a scope-refused rebuild is not journaled"
    );

    // The SAME rebuild through the seam with a scope that ADMITS its version applies.
    let mut admitted = std::collections::BTreeSet::new();
    admitted.insert(rb.migration.version.as_str().to_string());
    MigrationBackend::rebuild_one(
        &be,
        &rb.spec,
        &rb.migration,
        &ApprovalScope::Versions(admitted),
        "deployer",
    )
    .await
    .expect("the in-scope destructive rebuild applies");
    let info2 = be
        .actor()
        .query("PRAGMA main.table_info(events)")
        .await
        .expect("table_info post-rebuild");
    let n_type2 = info2
        .iter()
        .find(|r| r[1].as_deref() == Some("n"))
        .and_then(|r| r[2].clone())
        .expect("n column present");
    assert_eq!(
        n_type2.to_ascii_uppercase(),
        "TEXT",
        "the in-scope rebuild applied the type change, got {n_type2}"
    );
}

/// A destructive journal migration for a directly-constructed rebuild spec.
fn rebuild_migration(table: &str, spec: &SqliteRebuildSpec) -> Migration {
    use zero_migrate::model::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
    let flags = MigrationFlags {
        destructive: true,
        requires_approval: true,
        ..MigrationFlags::default()
    };
    let up = spec.new_table_create.clone();
    let checksum = Checksum::of(&ChecksumInput {
        up: &up,
        down: None,
        flags: &flags,
        owner_app: APP,
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: format!("sqlite_rebuild_{table}"),
        up,
        down: None,
        checksum,
        flags,
        owner_app: APP.to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}
