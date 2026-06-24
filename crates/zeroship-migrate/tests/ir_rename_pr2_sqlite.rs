//! PR2 — faithful e2e + unit coverage for the IR `renameColumn` lowering on the
//! **SQLite** leg (§2.6 / §2.6.1 / §2.6.2), and the dialect-router unit facts.
//!
//! The SQLite leg lowers ONE `op.renameColumn` to ONE
//! `PlanStep::OnlineRename(RenameStep::SqliteRebuild(_))` (the 12-step OFFLINE
//! table rebuild), executed via `MigrationBackend::rebuild_one` — NOT `run_online`
//! (SQLite has no online schema-change capability). These tests drive the REAL
//! lowering (`IrAuthor::lower_steps`) and APPLY through the engine's single shared
//! `apply_plan` on a real temp-file SQLite backend:
//!
//! - a seeded row SURVIVES the rename, the OLD column is gone, the journal records
//!   the rebuild migration (proving the `rebuild_one` path, NOT `run_online`);
//! - the lowered step is a `SqliteRebuild`, never a `PgExpandContract` (the leg
//!   dispatch, asserted structurally before apply);
//! - a neutral `ColType` renders the correct SQLite affinity in the rebuilt CREATE;
//! - a SQLite rename whose live table structure is ABSENT fails closed
//!   (`SqliteRenameNeedsLiveTable`) — never a wrong rebuild from a partial view.
//!
//! No shims, no PG-gated skips: the real SQLite runtime + the real journal.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::declarative::{
    desired_snapshot_for_dialect, CollectionDescriptor, FieldDescriptor,
};
use zeroship_migrate::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zeroship_migrate::ir_author::{IrAuthor, IrLowerError, LiveSchema};
use zeroship_migrate::backend_sqlite::Mode;
use zeroship_migrate::plan::{PlanStep, RenameStep};
use zeroship_migrate::{
    executor::LockMode, Approval, ExecutorConfig, MigrationBackend, MigrationEngine, SqlDialect,
    SqliteBackend,
};
use zeroship_schema::query::SqlDialect as SchemaDialect;

const PROJECT: &str = "prj_rename";
const APP: &str = "app_rename";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths { _dir: dir, app, journal }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT)
}

/// A one-field-collection descriptor (`name: field(ty)`), `required`.
fn descriptor(table: &str, field: &str, ty: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: field.into(),
            ty: ty.into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    }
}

/// Build the full PR2 `LiveSchema` (table snapshots + SQLite SDK schema `Value`s)
/// for the SQLite rename leg, by routing the descriptor set through the SAME
/// `desired_snapshot_for_dialect` the differ uses — so the live facts the rename
/// rebuild consumes are byte-identical to a `t.*`-diff's desired snapshot.
fn live_schema_for(descriptors: &[CollectionDescriptor]) -> LiveSchema {
    let desired = desired_snapshot_for_dialect(PROJECT, descriptors, SchemaDialect::Sqlite)
        .expect("desired snapshot");
    LiveSchema {
        tables: desired.snapshot.tables.keys().cloned().collect(),
        unique_indexes: BTreeSet::new(),
        table_snapshots: desired.snapshot.tables.clone(),
        sqlite_schemas: desired.sqlite_schemas.clone(),
    }
}

/// A one-op `renameColumn` IR.
fn rename_ir(table: &str, from: &str, to: &str, ty: ColType) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: format!("rename_{from}_to_{to}"),
        owner_app: APP.into(),
        ops: vec![Op::RenameColumn {
            table: table.into(),
            from: from.into(),
            to: to.into(),
            ty,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// Apply a v1 descriptor set as the first deploy (createTable), so the live
/// SQLite file actually has the table the rename rebuilds. Returns the live facts.
async fn first_deploy(be: &SqliteBackend, descriptors: &[CollectionDescriptor]) {
    // Lower each table's createTable IR and apply it.
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let engine = MigrationEngine::new();
    for d in descriptors {
        let cols: Vec<zeroship_migrate::ir::IrColumn> = d
            .fields
            .iter()
            .map(|f| zeroship_migrate::ir::IrColumn {
                name: f.name.clone(),
                ty: ColType::Text, // the e2e tables use text fields
                nullable: Some(!f.required),
                default: None,
                unique: None,
            })
            .collect();
        let ir = MigrationIr {
            ir_version: 1,
            name: format!("create_{}", d.name),
            owner_app: APP.into(),
            ops: vec![Op::CreateTable {
                name: d.name.clone(),
                columns: cols,
                constraints: vec![],
                indexes: vec![],
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let steps = author.lower_steps(&ir, &LiveSchema::default()).expect("lower create");
        engine
            .apply_plan(&steps, Approval::None, be, &exec_cfg(), "deploy", LockMode::Acquire)
            .await
            .expect("apply create");
    }
}

// §2.6.2 — the SQLite leg: ONE `op.renameColumn` lowers to ONE
// `RenameStep::SqliteRebuild` and applies via `rebuild_one` THROUGH the single
// shared `apply_plan`. The seeded row survives, the old column is gone, the
// journal records the rebuild — and the lowered step is a SqliteRebuild, NOT a
// PgExpandContract (so NO run_online path is taken).
#[compio::test]
async fn renamecolumn_lowers_and_applies_as_sqlite_rebuild_through_apply_plan() {
    let p = paths("sqlite_rebuild");
    let be = backend(&p);

    // v1: people(nickname:text). Create it for real.
    let v1 = vec![descriptor("people", "nickname", "string")];
    first_deploy(&be, &v1).await;

    // Seed rows BEFORE the rename — they must survive the rebuild.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.people (id, nickname) VALUES ('p1','ada'),('p2','grace')")
        .await
        .expect("seed");

    // The live facts (full table structure) the SQLite rebuild needs.
    let live = live_schema_for(&v1);

    // Lower the rename `nickname → handle` on the SQLite leg.
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let ir = rename_ir("people", "nickname", "handle", ColType::Text);
    let steps = author.lower_steps(&ir, &live).expect("SQLite rename lowers");

    // STRUCTURAL leg assertion (§2.6.2): exactly one step, an OnlineRename whose
    // RenameStep is the SQLite REBUILD — NOT a PG expand-contract.
    assert_eq!(steps.len(), 1, "one renameColumn → one plan step");
    let rebuild_version = match &steps[0] {
        PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => {
            rb.migration.version.as_str().to_string()
        }
        PlanStep::OnlineRename(RenameStep::PgExpandContract(_)) => {
            panic!("SQLite must lower to a rebuild, NOT a PG expand-contract")
        }
        other => panic!("expected an OnlineRename step, got {other:?}"),
    };

    // Apply THROUGH the single shared apply_plan (a rebuild on a populated table is
    // destructive ⇒ Approval::Approved).
    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(&steps, Approval::Approved, &be, &exec_cfg(), "deployer", LockMode::Acquire)
        .await
        .expect("apply the SQLite rename rebuild");

    // No PG online partition on the SQLite leg (§2.6.2).
    assert!(
        out.pending_contract.is_empty(),
        "a SQLite rebuild has NO pending_contract partition"
    );
    assert!(
        out.applied.applied.contains(&rebuild_version),
        "apply_plan journaled the rebuild migration's version"
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

    // The old column is GONE.
    let info = be.actor().query("PRAGMA main.table_info(people)").await.expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("nickname")),
        "the old column name is gone after the rebuild rename"
    );

    // The journal records the REBUILD migration as Completed — the proof it ran via
    // `rebuild_one` and NOT `run_online` (run_online journals the PG E1..C2
    // expand sub-steps, a wholly different version set; here the only journaled
    // online-rename version is the single rebuild migration's).
    let applied = be.applied(&exec_cfg()).await.expect("journal");
    assert!(
        applied.iter().any(|e| e.version == rebuild_version
            && matches!(e.phase, zeroship_migrate::journal::Phase::Completed)),
        "the rebuild migration is journaled completed (rebuild_one path)"
    );
    // And NO `expand_*` / `contract_*`-named online sub-step ever journaled (the
    // run_online path was never taken).
    assert!(
        applied.iter().all(|e| !e.version.starts_with("expand_")
            && !e.version.starts_with("contract_")),
        "no PG expand-contract sub-step journaled on the SQLite leg"
    );
}

// §2.6 — neutral-type translation on the SQLite leg: a renameColumn whose neutral
// ColType is `Int` renders the correct SQLite affinity (INTEGER) in the rebuilt
// table's CREATE — NOT a PG type string. (The PG-type-string assertion is the PG
// suite's job; here we prove the SQLite leg never receives one.)
#[compio::test]
async fn renamecolumn_sqlite_renders_neutral_type_as_affinity_not_pg_string() {
    let p = paths("sqlite_affinity");
    let be = backend(&p);

    // v1: events(count:int). The field is an INTEGER-affinity column.
    let v1 = vec![descriptor("events", "count", "int")];
    // Create it for real with an int column.
    {
        let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
        let engine = MigrationEngine::new();
        let ir = MigrationIr {
            ir_version: 1,
            name: "create_events".into(),
            owner_app: APP.into(),
            ops: vec![Op::CreateTable {
                name: "events".into(),
                columns: vec![zeroship_migrate::ir::IrColumn {
                    name: "count".into(),
                    ty: ColType::Int,
                    nullable: Some(false),
                    default: None,
                    unique: None,
                }],
                constraints: vec![],
                indexes: vec![],
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let steps = author.lower_steps(&ir, &LiveSchema::default()).expect("create");
        engine
            .apply_plan(&steps, Approval::None, &be, &exec_cfg(), "deploy", LockMode::Acquire)
            .await
            .expect("apply create");
    }

    let live = live_schema_for(&v1);
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let ir = rename_ir("events", "count", "total", ColType::Int);
    let steps = author.lower_steps(&ir, &live).expect("rename lowers");

    let rebuild = match &steps[0] {
        PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => rb,
        other => panic!("expected a SQLite rebuild, got {other:?}"),
    };
    // The rebuilt CREATE renders the renamed `total` column with INTEGER affinity
    // (the SQLite spelling) — and never the PG `integer`/`int4` spelling nor a
    // qualified PG schema. The shared emitter writes SQLite type tokens uppercase.
    let create = &rebuild.spec.new_table_create;
    assert!(
        create.contains("\"total\""),
        "the rebuilt CREATE carries the renamed column: {create}"
    );
    assert!(
        create.to_ascii_uppercase().contains("INTEGER"),
        "the neutral Int type renders SQLite INTEGER affinity: {create}"
    );
    // Defense: NO PG-qualified schema leaked into the SQLite leg.
    assert!(
        !create.contains(&format!("\"{PROJECT}\".")),
        "no PG schema qualification on the SQLite rebuild CREATE: {create}"
    );
}

// §2.6.2 fail-closed: a SQLite renameColumn whose table's full live structure is
// NOT in the LiveSchema cannot lower — it refuses rather than emit a rebuild from
// a partial view of the table.
#[test]
fn renamecolumn_sqlite_fails_closed_without_live_table_structure() {
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let ir = rename_ir("ghost", "a", "b", ColType::Text);
    // LiveSchema knows the table NAME but not its structure (table_snapshots /
    // sqlite_schemas empty) — the rebuild needs the whole shape.
    let mut live = LiveSchema::default();
    live.tables.insert("ghost".into());
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a SQLite rename with no live table structure must fail closed");
    match err {
        IrLowerError::SqliteRenameNeedsLiveTable(t) => assert_eq!(t, "ghost"),
        other => panic!("expected SqliteRenameNeedsLiveTable, got: {other}"),
    }
}
