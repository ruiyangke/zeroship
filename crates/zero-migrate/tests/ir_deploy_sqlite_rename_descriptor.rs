//! Online-rename GO-LIVE (SQLite leg) — faithful e2e through the REAL deploy/
//! apply entry point [`apply_bundle_ir_sqlite`], NOT a hand-built `lower_steps` call.
//!
//! Earlier the SQLite IR rename was engine-proven but never deploy-wired (no path
//! constructed a SQLite-dialect `LiveSchema` with the `sqlite_schemas` SDK `Value`s
//! the rebuild needs). This test proves the wiring: a `renameColumn` IR envelope
//! deployed through `apply_bundle_ir_sqlite` applies as a 12-step REBUILD on a real
//! temp-file SQLite DB — rows mirrored, old column gone, journal records the rebuild
//! — using ONLY the app's descriptor set to build the live facts (the production/dev
//! shape).
//!
//! No shims, no PG-gated skips: the real SQLite runtime + the real journal + the real
//! load/guard/apply path.

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};
use zero_migrate::{
    apply_bundle_ir_sqlite, Approval, ExecutorConfig, GuardConfig, MigrationBackend, MigrationIr,
    PolicyProfile, SqliteBackend, SqliteIrApplyError, resolve_create_table_policy,
};

const PROJECT: &str = "prj_rename_descriptor";
const APP: &str = "app_rename_descriptor";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
    migrations: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    let migrations = dir.path().join("migrations");
    std::fs::create_dir_all(&migrations).expect("mkdir migrations");
    Paths { _dir: dir, app, journal, migrations }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT)
}

/// A one-text-field-collection descriptor (`<field>: string`, required).
fn descriptor(table: &str, field: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: field.into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    runtime_options: Default::default(),
    }
}

fn write_ir(p: &Paths, file: &str, body: &str) {
    let ir: MigrationIr = serde_json::from_str(body).expect("test IR parses");
    let resolved =
        resolve_create_table_policy(&ir, &PolicyProfile::confined()).expect("test IR resolves");
    let body = serde_json::to_string(&resolved).expect("resolved test IR serializes");
    std::fs::write(p.migrations.join(file), body).expect("write ir file");
}

fn clear_migrations(p: &Paths) {
    for entry in std::fs::read_dir(&p.migrations).expect("read migrations dir") {
        let path = entry.expect("dir entry").path();
        let _ = std::fs::remove_file(path);
    }
}

// The headline: a `renameColumn` deploy COMPLETES as a SQLite rebuild through the
// REAL `apply_bundle_ir_sqlite` entry point. A row survives, the old column is gone,
// the journal records the rebuild migration — and there is NO pending_contract
// (SQLite is offline-rebuild, not the PG cross-deploy expand-contract).
#[compio::test]
async fn deploy_renamecolumn_completes_as_rebuild_on_real_sqlite() {
    let p = paths("rename_golive");
    let be = backend(&p);

    // Deploy #1: createTable people(nickname) via an IR envelope — the live table the
    // rename rebuilds. The descriptor set is the v1 schema.
    let v1 = [descriptor("people", "nickname")];
    let create = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[
            {"name":"nickname","type":"text","nullable":false}
        ]}
    ]}"#;
    write_ir(&p, "0001_create_people.ir.json", create);
    let out1 = apply_bundle_ir_sqlite(
        &be,
        PROJECT,
        APP,
        &v1,
        &p.migrations,
        &exec_cfg(),
        &GuardConfig::confined(PROJECT),
        &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable deploy must succeed");
    assert_eq!(out1.applied.len(), 1, "the createTable applied");

    // Seed rows BEFORE the rename — they must survive the rebuild.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.people (id, created_at, updated_at, version, nickname) VALUES \
             ('p1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'ada'), \
             ('p2','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'grace')",
        )
        .await
        .expect("seed");

    // Deploy #2: renameColumn nickname → handle. The post-rename descriptor set is
    // v2 (the field renamed). The rename rebuild is destructive on a populated table,
    // so it needs Approval::Approved (the approved go-live surface).
    clear_migrations(&p);
    let rename = r#"{"ir_version":1,"name":"rename_nickname","ops":[
        {"op":"renameColumn","table":"people","from":"nickname","to":"handle","type":"text"}
    ]}"#;
    write_ir(&p, "0002_rename_nickname.ir.json", rename);
    let v2 = [descriptor("people", "nickname")]; // live facts are the PRE-rename shape
    let out2 = apply_bundle_ir_sqlite(
        &be,
        PROJECT,
        APP,
        &v2,
        &p.migrations,
        &exec_cfg(),
        &GuardConfig::confined(PROJECT),
        &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect("renameColumn deploy must complete as a rebuild");
    assert_eq!(
        out2.applied.len(),
        1,
        "the rename applied exactly one (rebuild) migration"
    );

    // The data followed the rename: `handle` carries the seeded values.
    let vals = be
        .actor()
        .query("SELECT handle FROM main.people ORDER BY id")
        .await
        .expect("read handle");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["ada", "grace"],
        "the seeded rows survive the rename and live under the new column"
    );

    // The old column is GONE (the rebuild replaced the table).
    let info = be.actor().query("PRAGMA main.table_info(people)").await.expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("nickname")),
        "the old column name is gone after the rebuild rename"
    );
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("handle")),
        "the new column name is present after the rebuild rename"
    );

    // The journal records the rebuild migration as Completed (the rebuild_one path).
    let applied = be.applied(&exec_cfg()).await.expect("journal");
    assert!(
        applied
            .iter()
            .filter(|e| matches!(e.phase, zero_migrate::apply::journal::Phase::Completed))
            .count()
            >= 2,
        "both the createTable and the rebuild are journaled completed"
    );
}

// SQLite go-live descriptor semantics — a `renameColumn`
// deployed with a descriptor set derived from the app's registerModel = the POST-deploy
// DESIRED schema (post-rename: only `handle` exists, `nickname` is GONE) FAILS CLOSED,
// with NO data loss. This is the production-NATURAL descriptor set: a real caller
// derives `for_sqlite_descriptors` from registerModel (the end-state), in which the
// rename's `from` column no longer exists — so the rebuild author cannot find the live
// `from` column facts and refuses (`RenameNeedsLiveColumn` / `SqliteRenameNeedsLiveTable`)
// rather than emit a wrong rebuild. This pins the DESCRIPTOR-SET CONTRACT (role B in
// ir_apply.rs): a SQLite rename needs the PRE-rename column facts; the post-rename
// desired descriptor set is the WRONG source, and the leg fails closed (no data loss)
// until the production wiring wave sources the pre-rename facts from a real pre-deploy
// catalog/snapshot read. (Distinct from the multi-file intermediate test: here it is a
// SINGLE rename file applied against a live table, with a post-rename descriptor set.)
#[compio::test]
async fn deploy_post_rename_descriptor_set_fails_closed_on_real_sqlite() {
    let p = paths("post_rename_descriptors");
    let be = backend(&p);

    // Deploy #1: createTable people(nickname) with the v1 (pre-rename) descriptor set —
    // a real live table with seeded rows.
    let v1 = [descriptor("people", "nickname")];
    let create = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[
            {"name":"nickname","type":"text","nullable":false}
        ]}
    ]}"#;
    write_ir(&p, "0001_create_people.ir.json", create);
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable deploy must succeed");

    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.people (id, created_at, updated_at, version, nickname) VALUES \
             ('p1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'ada'), \
             ('p2','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'grace')",
        )
        .await
        .expect("seed");

    // Deploy #2: renameColumn nickname → handle, but the descriptor set is the
    // registerModel-derived POST-rename DESIRED schema — the field is ALREADY `handle`,
    // and `nickname` (the rename's `from`) is GONE. This is what a production caller
    // would naturally pass; it must FAIL CLOSED.
    clear_migrations(&p);
    let rename = r#"{"ir_version":1,"name":"rename_nickname","ops":[
        {"op":"renameColumn","table":"people","from":"nickname","to":"handle","type":"text"}
    ]}"#;
    write_ir(&p, "0002_rename_nickname.ir.json", rename);
    let post_rename_desired = [descriptor("people", "handle")];
    let err = apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &post_rename_desired, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect_err(
        "a rename deployed from the POST-rename (registerModel-derived) descriptor set must \
         FAIL CLOSED — the rebuild author cannot find the live `from` column to copy",
    );
    assert!(
        matches!(err, SqliteIrApplyError::Ir { .. }),
        "expected a fail-closed IR lower error (rename needs the PRE-rename live `from` \
         column facts; the post-rename desired descriptor set lacks them), got {err:?}"
    );

    // NO DATA LOSS: the live table is UNTOUCHED — still `nickname`, both rows intact.
    let info = be.actor().query("PRAGMA main.table_info(people)").await.expect("table_info");
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("nickname")),
        "the live `nickname` column is untouched — the fail-closed rename did no DDL"
    );
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("handle")),
        "no wrong rebuild: `handle` must NOT exist (the rename was refused)"
    );
    let vals = be
        .actor()
        .query("SELECT nickname FROM main.people ORDER BY id")
        .await
        .expect("read nickname");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["ada", "grace"],
        "the seeded rows are intact — fail-closed means zero data loss"
    );
}

// A SQLite rename through the REAL entry point under Approval::None is REFUSED
// (the rebuild on a populated table is destructive) — no go-live, the old column is
// intact. This is the SQLite peer of the PG routine-deploy refusal.
#[compio::test]
async fn deploy_renamecolumn_refused_under_no_approval_on_real_sqlite() {
    let p = paths("rename_refused");
    let be = backend(&p);

    let v1 = [descriptor("widgets", "label")];
    let create = r#"{"ir_version":1,"name":"create_widgets","ops":[
        {"op":"createTable","name":"widgets","columns":[
            {"name":"label","type":"text","nullable":false}
        ]}
    ]}"#;
    write_ir(&p, "0001_create_widgets.ir.json", create);
    apply_bundle_ir_sqlite(
        &be,
        PROJECT,
        APP,
        &v1,
        &p.migrations,
        &exec_cfg(),
        &GuardConfig::confined(PROJECT),
        &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable deploy must succeed");

    clear_migrations(&p);
    let rename = r#"{"ir_version":1,"name":"rename_label","ops":[
        {"op":"renameColumn","table":"widgets","from":"label","to":"title","type":"text"}
    ]}"#;
    write_ir(&p, "0002_rename_label.ir.json", rename);
    let err = apply_bundle_ir_sqlite(
        &be,
        PROJECT,
        APP,
        &v1,
        &p.migrations,
        &exec_cfg(),
        &GuardConfig::confined(PROJECT),
        &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect_err("a rename rebuild must be refused under Approval::None");
    assert!(
        matches!(err, SqliteIrApplyError::Apply(_)),
        "the refusal is an apply-time approval rejection, got {err:?}"
    );

    // Nothing changed: the old column is intact.
    let info = be.actor().query("PRAGMA main.table_info(widgets)").await.expect("table_info");
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("label")),
        "the old `label` column is intact — the refused rename touched nothing"
    );
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("title")),
        "the new `title` column must NOT exist — the rename was refused"
    );
}

/// A one-text-field descriptor carrying a UNIQUE index on that field.
fn descriptor_unique(table: &str, field: &str, index: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: field.into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![zero_migrate::render::declarative::IndexDescriptor {
            name: index.into(),
            columns: vec![field.into()],
            unique: true,
        }],
        runtime_options: Default::default(),
    }
}

// The SQLite peer of
// `deploy_migrate_refuses_understated_unique_drop_from_live_fact`. A `dropIndex`
// that LIES about uniqueness (`unique:false`) on an actually-UNIQUE index must be
// REFUSED on the SQLite go-live path under Approval::None — the authoritative source
// is the descriptor-derived unique-index set the deploy threads into the SQLite
// `LiveSchema` (was discarded pre-fix, reopening the approval-gate hole the PG path
// closes). The index survives; nothing applied.
#[compio::test]
async fn deploy_understated_unique_drop_refused_on_real_sqlite() {
    let p = paths("uniq_drop_refused");
    let be = backend(&p);

    // Deploy #1: createTable users(email) + a UNIQUE index on email — op.* only.
    let v1 = [descriptor_unique("users", "email", "users_email_uniq")];
    let create = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[{"name":"email","type":"text","nullable":false}]},
        {"op":"createIndex","table":"users","columns":[{"kind":"column","name":"email"}],"name":"users_email_uniq","unique":true}
    ]}"#;
    write_ir(&p, "0001_create_users.ir.json", create);
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable + unique createIndex deploy must succeed");

    // Deploy #2: a dropIndex understating the unique index as `unique:false`. The
    // descriptor-derived unique set must override the hint ⇒ destructive ⇒ refused
    // under Approval::None.
    clear_migrations(&p);
    let drop = r#"{"ir_version":1,"name":"drop_uniq","ops":[
        {"op":"dropIndex","name":"users_email_uniq","table":"users","unique":false}
    ]}"#;
    write_ir(&p, "0002_drop_uniq.ir.json", drop);
    let err = apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect_err("an understated-unique drop of a descriptor-unique index must be refused");
    assert!(
        matches!(err, SqliteIrApplyError::Apply(_)),
        "expected an apply-time destructive refusal, got {err:?}"
    );

    // The index SURVIVES the refused drop (nothing applied).
    let idx = be
        .actor()
        .query("SELECT name FROM main.sqlite_master WHERE type='index' AND name='users_email_uniq'")
        .await
        .expect("query sqlite_master");
    assert!(
        idx.iter().any(|r| r[0].as_deref() == Some("users_email_uniq")),
        "the unique index must SURVIVE the refused drop"
    );
}

// The descriptor-set contract is FAIL-CLOSED. The
// SQLite leg's structural rebuild facts come from the END-STATE descriptor set (a
// `sqlite_master` read can't recover the SDK-`Value` facets), so a multi-file deploy
// whose LATER file depends on an INTERMEDIATE structural state produced by an EARLIER
// file in the same directory CANNOT be represented — the rename's `from` column is not
// in the descriptor-pinned live facts. This must FAIL CLOSED (refuse to emit a wrong
// rebuild), never silently apply against the wrong shape. (Pins the chosen contract:
// single-structural-op-per-directory / end-state descriptors; multi-file intermediate
// state is rejected.)
#[compio::test]
async fn deploy_multi_file_intermediate_rename_fails_closed_on_real_sqlite() {
    let p = paths("multi_file_intermediate");
    let be = backend(&p);

    // ONE deploy directory with TWO files: 0001 createTable people(nickname) and
    // 0002 renameColumn nickname → handle. The descriptor set is the END-STATE
    // (post-rename: `handle`), so 0002's rename `from=nickname` is NOT in the live
    // facts the descriptor set pins.
    let create = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[{"name":"nickname","type":"text","nullable":false}]}
    ]}"#;
    let rename = r#"{"ir_version":1,"name":"rename_nickname","ops":[
        {"op":"renameColumn","table":"people","from":"nickname","to":"handle","type":"text"}
    ]}"#;
    write_ir(&p, "0001_create_people.ir.json", create);
    write_ir(&p, "0002_rename_nickname.ir.json", rename);
    // The end-state descriptor set: the column is ALREADY `handle` (post-rename).
    let end_state = [descriptor("people", "handle")];

    let err = apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &end_state, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect_err(
        "a multi-file deploy needing the post-createTable intermediate shape must FAIL CLOSED \
         (the end-state descriptors don't carry the pre-rename `nickname` column)",
    );
    assert!(
        matches!(err, SqliteIrApplyError::Ir { .. }),
        "expected a fail-closed IR lower error (rename needs the live `from` column), got {err:?}"
    );

    // Nothing partial: the table was created by 0001 but the rename did NOT apply, so
    // the column is still `nickname` (no silent wrong rebuild to `handle`).
    let info = be.actor().query("PRAGMA main.table_info(people)").await.expect("table_info");
    assert!(
        info.iter().any(|r| r[1].as_deref() == Some("nickname")),
        "0001 created the table with `nickname`; the fail-closed 0002 must not have rebuilt it"
    );
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("handle")),
        "the rename must NOT have applied — no silent wrong rebuild"
    );
}

/// A two-text-field descriptor (for the hero's post-add live shape).
fn descriptor3(table: &str, a: &str, b: &str, c: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor { name: a.into(), ty: "string".into(), required: false, ..Default::default() },
            FieldDescriptor { name: b.into(), ty: "string".into(), required: false, ..Default::default() },
            FieldDescriptor { name: c.into(), ty: "string".into(), required: false, ..Default::default() },
        ],
        indexes: vec![],
    runtime_options: Default::default(),
    }
}

// EVIDENCE (SQLite leg) — the headline "op.* replaces raw-SQL authoring" proof on
// SQLite, through the REAL deploy entry point `apply_bundle_ir_sqlite`: the hero
// DDL+backfill (addColumn first_name/last_name + a splitPart backfill + dropColumn
// name) authored ENTIRELY as op.* IR envelope, NO raw `.sql` anywhere, applies on real
// SQLite and produces the split columns. The peer of the PG `deploy_migrate_no_raw_
// sql_hero_ddl_backfill_applies_pg` evidence test.
#[compio::test]
async fn deploy_no_raw_sql_hero_ddl_backfill_applies_sqlite() {
    let p = paths("hero_no_raw");
    let be = backend(&p);

    // Deploy #1: createTable people(name) + seed two "first last" names — op.* only.
    let v1 = [descriptor("people", "name")];
    let create = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[{"name":"name","type":"text","nullable":true}]}
    ]}"#;
    write_ir(&p, "0001_create_people.ir.json", create);
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &v1, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::None,
    )
    .await
    .expect("createTable deploy must succeed");

    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.people (id, name, created_at, updated_at, version) VALUES \
             ('p1','Ada Lovelace','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1), \
             ('p2','Grace Hopper','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1)",
        )
        .await
        .expect("seed");

    // Deploy #2 (approved): the op.*-authored hero DDL+backfill — NO raw SQL.
    clear_migrations(&p);
    let hero = r#"{"ir_version":1,"name":"split_name","ops":[
        {"op":"addColumn","table":"people","column":"first_name","type":"text"},
        {"op":"addColumn","table":"people","column":"last_name","type":"text"},
        {"op":"backfill","table":"people","cursorColumn":"id","batchSize":50,
         "set":{
            "first_name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"name"},{"node":"literal","value":" "},{"node":"literal","value":1}]},
            "last_name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"name"},{"node":"literal","value":" "},{"node":"literal","value":2}]}
         },"name":"split_name_bf"},
        {"op":"dropColumn","table":"people","column":"name"}
    ]}"#;
    write_ir(&p, "0002_split_name.ir.json", hero);
    // EVIDENCE: the bundle contains NO raw `.sql` file.
    assert!(
        std::fs::read_dir(&p.migrations)
            .unwrap()
            .filter_map(Result::ok)
            .all(|e| !e.file_name().to_string_lossy().ends_with(".sql")),
        "the op.*-authored hero bundle must contain NO raw .sql file"
    );
    // The live facts: after the two addColumn ops the backfill references `name` +
    // writes first_name/last_name — the live descriptor shape carries all three.
    let live = [descriptor3("people", "name", "first_name", "last_name")];
    apply_bundle_ir_sqlite(
        &be, PROJECT, APP, &live, &p.migrations, &exec_cfg(),
        &GuardConfig::confined(PROJECT), &PolicyProfile::confined(), Approval::Approved,
    )
    .await
    .expect("the op.*-authored DDL+backfill hero must apply on SQLite with no raw SQL");

    // The split transform ran and `name` is gone.
    let rows = be
        .actor()
        .query("SELECT first_name, last_name FROM main.people ORDER BY id")
        .await
        .expect("read split columns");
    assert_eq!(
        rows.iter().map(|r| (r[0].clone(), r[1].clone())).collect::<Vec<_>>(),
        vec![
            (Some("Ada".into()), Some("Lovelace".into())),
            (Some("Grace".into()), Some("Hopper".into())),
        ],
        "the op.* splitPart backfill split the names on the real SQLite deploy path"
    );
    let info = be.actor().query("PRAGMA main.table_info(people)").await.expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("name")),
        "the op.* dropColumn removed `name`"
    );
}
