//! PHASE 3b — the SQLite 12-step table REBUILD, end-to-end against REAL temp-file
//! SQLite through the hardened `SqliteBackend` (design §2.4). The faithful path:
//! the real `DeclarativeAuthor` builds the rebuild spec from a descriptor diff, and
//! the real backend executes the 12-step under confinement (CreatorUp rebuild DDL,
//! EngineJournal PRAGMA/check/journal, `foreign_keys` toggles straddling the txn).
//!
//! Coverage (the P3b gate):
//! - type change with rows preserved + journaled + indexes recreated;
//! - nullability tighten (NULL → NOT NULL) and a column RENAME via rebuild;
//! - a goodie (mask/encrypted) survives the rebuild — the sentinel still recovers
//!   via the P5 drift snapshot;
//! - FK integrity: a rebuild whose `foreign_key_check` would FAIL aborts with a
//!   typed error, the original table is intact, and `foreign_keys` is back ON;
//! - confinement during rebuild: a creator `up` cannot toggle `foreign_keys` nor
//!   reach `_mig`; after a rebuild the connection has `foreign_keys=ON`;
//! - failure path: an aborting rebuild leaves no wedge (the next apply succeeds) and
//!   `foreign_keys` is back ON.

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::backend_sqlite::Mode;
use zeroship_migrate::{
    desired_snapshot, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor, IndexDescriptor,
    Migration, RebuildError, SchemaSnapshot, SqliteBackend, SqliteRebuild, SqliteRebuildSpec,
};
use zeroship_schema::query::SqlDialect;

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
        .map(|s| s == "1")
        .unwrap_or(false)
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
            && e.phase == zeroship_migrate::journal::Phase::Completed),
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
    use zeroship_migrate::RenameHint;

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
//     still recover via the P5 drift snapshot (the new CREATE carries them inline).
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
        rb.spec.new_table_create.contains("/* __zsmask:")
            && rb.spec.new_table_create.contains("/* zsenc:"),
        "the rebuilt CREATE must carry the inline mask + enc sentinels: {}",
        rb.spec.new_table_create
    );
    be.rebuild_one(&rb.spec, &rb.migration, "deployer")
        .await
        .expect("goodie rebuild applies");

    // The drift snapshot recovers the sentinels from the rebuilt table's
    // sqlite_master.sql (P5 §2.7).
    let snap = be.snapshot_schema_sqlite().await.expect("snapshot");
    let t = snap.tables.get("accounts").expect("accounts in snapshot");
    let secret = t.columns.iter().find(|c| c.name == "secret").expect("secret");
    assert!(
        secret
            .comment_sentinel
            .as_deref()
            .or(secret.encryption_sentinel.as_deref())
            .map(|s| s.contains("zsenc:"))
            .unwrap_or(false),
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
            .map(|s| s.contains("__zsmask:"))
            .unwrap_or(false),
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
        new_table_create: "CREATE TABLE \"child__zsrebuild\" (\
            \"id\" TEXT PRIMARY KEY, \
            \"parent_id\" TEXT REFERENCES \"parent\" (\"id\"))"
            .into(),
        copy_columns: vec![
            ("id".into(), "id".into()),
            ("parent_id".into(), "parent_id".into()),
        ],
        recreate_objects: vec![],
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
             AND name='child__zsrebuild'",
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
        "INSERT INTO \"_mig\".schema_migrations (event_seq, version, name, checksum, \
         applied_by, phase, outcome, kind) VALUES (999, 'x', 'x', 'x', 'x', 'completed', \
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
        new_table_create: "CREATE TABLE \"t__zsrebuild\" (\"id\" TEXT PRIMARY KEY, \"v\" TEXT)"
            .into(),
        copy_columns: vec![("id".into(), "id".into()), ("v".into(), "v".into())],
        // A bogus index over a column that does not exist → CREATE INDEX errors.
        recreate_objects: vec![
            "CREATE INDEX \"t_bogus_idx\" ON \"t\" (\"does_not_exist\")".into(),
        ],
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
        .query("SELECT name FROM main.sqlite_master WHERE type='table' AND name='t__zsrebuild'")
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
// Helpers: build a Migration without the declarative author (for direct specs).
// ---------------------------------------------------------------------------

fn simple_migration(name: &str, up: &str) -> Migration {
    use zeroship_migrate::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
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
    }
}

/// A destructive journal migration for a directly-constructed rebuild spec.
fn rebuild_migration(table: &str, spec: &SqliteRebuildSpec) -> Migration {
    use zeroship_migrate::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
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
    }
}
