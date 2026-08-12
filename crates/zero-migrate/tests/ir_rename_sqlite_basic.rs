//! Faithful e2e + unit coverage for the IR `renameColumn` lowering on the
//! **`SQLite`** leg, and the dialect-router unit facts.
//!
//! The `SQLite` leg lowers ONE `op.renameColumn` to ONE
//! `PlanStep::OnlineRename(RenameStep::SqliteRebuild(_))` (the 12-step OFFLINE
//! table rebuild), executed via `MigrationBackend::rebuild_one` — NOT `run_online`
//! (`SQLite` has no online schema-change capability). These tests drive the REAL
//! lowering (`IrAuthor::lower_steps`) and APPLY through the engine's single shared
//! `apply_plan` on a real temp-file `SQLite` backend:
//!
//! - a seeded row SURVIVES the rename, the OLD column is gone, the journal records
//!   the rebuild migration (proving the `rebuild_one` path, NOT `run_online`);
//! - the lowered step is a `SqliteRebuild`, never a `PgExpandContract` (the leg
//!   dispatch, asserted structurally before apply);
//! - a neutral `ColType` renders the correct `SQLite` affinity in the rebuilt CREATE;
//! - a `SQLite` rename whose live table structure is ABSENT fails closed
//!   (`SqliteRenameNeedsLiveTable`) — never a wrong rebuild from a partial view.
//!
//! No shims, no PG-gated skips: the real `SQLite` runtime + the real journal.

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::model::ir::{ColType, IrFlagsOverride, MigrationIr, Op};
use zero_migrate::render::declarative::{
    desired_snapshot_for_dialect, CollectionDescriptor, FieldDescriptor,
};
use zero_migrate::render::lower::{IrAuthor, IrLowerError, LiveSchema};
use zero_migrate::schema::query::SqlDialect as SchemaDialect;
use zero_migrate::{
    apply::executor::LockMode, resolve_create_table_policy, Approval, ExecutorConfig,
    MigrationBackend, MigrationEngine, SqlDialect, SqliteBackend,
};
use zero_migrate::{PlanStep, RenameStep};

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
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT))
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
        runtime_options: Default::default(),
    }
}

/// Build the full `LiveSchema` (table snapshots + `SQLite` SDK schema `Value`s)
/// for the `SQLite` rename leg, by routing the descriptor set through the SAME
/// `desired_snapshot_for_dialect` the differ uses — so the live facts the rename
/// rebuild consumes are byte-identical to a `t.*`-diff's desired snapshot.
fn live_schema_for(descriptors: &[CollectionDescriptor]) -> LiveSchema {
    let effective = support::confined_charter();
    let desired =
        desired_snapshot_for_dialect(PROJECT, descriptors, SchemaDialect::Sqlite, &effective)
            .expect("desired snapshot");
    // Every table is owned by the deploying app (`APP`) — the same-app rename case.
    let table_ownership = desired
        .snapshot
        .tables
        .keys()
        .map(|t| (t.clone(), APP.to_string()))
        .collect();
    LiveSchema {
        tables: desired.snapshot.tables.keys().cloned().collect(),
        unique_indexes: BTreeSet::new(),
        table_snapshots: desired.snapshot.tables.clone(),
        partitions: desired.snapshot.partitions.clone(),
        views: desired.snapshot.views.clone(),
        sequences: desired.snapshot.sequences.clone(),
        extensions: desired.snapshot.extensions.clone(),
        functions: desired.snapshot.functions.clone(),
        policies: desired.snapshot.policies.clone(),
        triggers: desired.snapshot.triggers.clone(),
        schemas: desired.snapshot.schemas.clone(),
        sqlite_schemas: desired.sqlite_schemas,
        table_ownership,
        logical_columns: Default::default(),
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
            schema: None,
            existence_guard: None,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// Apply a v1 descriptor set as the first deploy (createTable), so the live
/// `SQLite` file actually has the table the rename rebuilds. Returns the live facts.
async fn first_deploy(be: &SqliteBackend, descriptors: &[CollectionDescriptor]) {
    // Lower each table's createTable IR and apply it.
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let engine = MigrationEngine::new();
    for d in descriptors {
        let cols: Vec<zero_migrate::model::ir::IrColumn> = d
            .fields
            .iter()
            .map(|f| zero_migrate::model::ir::IrColumn {
                name: f.name.clone(),
                ty: ColType::Text, // the e2e tables use text fields
                nullable: Some(!f.required),
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            })
            .collect();
        let ir = MigrationIr {
            ir_version: 1,
            name: format!("create_{}", d.name),
            owner_app: APP.into(),
            ops: vec![Op::CreateTable {
                name: d.name.clone(),
                columns: cols,
                primary_key: None,
                constraints: vec![],
                indexes: vec![],

                partition_by: None,

                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let ir = resolve_create_table_policy(&ir, &support::confined_charter(), PROJECT)
            .expect("test IR resolves");
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("lower create");
        engine
            .apply_plan(
                &steps,
                Approval::None,
                be,
                &exec_cfg(),
                "deploy",
                LockMode::Acquire,
            )
            .await
            .expect("apply create");
    }
}

// The SQLite leg: ONE `op.renameColumn` lowers to ONE
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
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.people (id, created_at, updated_at, version, nickname) VALUES \
             ('p1','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'ada'), \
             ('p2','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z',1,'grace')",
        )
        .await
        .expect("seed");
    be.actor()
        .exec("CREATE INDEX people_nickname_idx ON people (nickname)")
        .await
        .expect("create dependent nickname index");

    // The live facts (full table structure) the SQLite rebuild needs.
    let mut live = live_schema_for(&v1);
    let catalog = be
        .snapshot_schema_sqlite()
        .await
        .expect("introspect the real pre-rename table");
    live.tables = catalog.tables.keys().cloned().collect();
    live.table_snapshots = catalog.tables;

    // Lower the rename `nickname → handle` on the SQLite leg.
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let ir = rename_ir("people", "nickname", "handle", ColType::Text);
    let steps = author
        .lower_steps(&ir, &live)
        .expect("SQLite rename lowers");

    // STRUCTURAL leg assertion: exactly one step, an OnlineRename whose
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

    // Snapshot the journal's completed-version set BEFORE the rename applies (it
    // already carries the first_deploy createTable). The rename's exact journal
    // footprint is the set-difference after vs. before.
    let before: std::collections::BTreeSet<String> = be
        .applied(&exec_cfg())
        .await
        .expect("journal before rename")
        .into_iter()
        .filter(|e| matches!(e.phase, zero_migrate::apply::journal::Phase::Completed))
        .map(|e| e.version.as_str().to_string())
        .collect();

    // Apply THROUGH the single shared apply_plan (a rebuild on a populated table is
    // destructive ⇒ Approval::Approved).
    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &be,
            &exec_cfg(),
            "deployer",
            LockMode::Acquire,
        )
        .await
        .expect("apply the SQLite rename rebuild");

    // No PG online partition on the SQLite leg.
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
    let info = be
        .actor()
        .query("PRAGMA main.table_info(people)")
        .await
        .expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("nickname")),
        "the old column name is gone after the rebuild rename"
    );
    let dependent_index = be
        .actor()
        .query(
            "SELECT sql FROM main.sqlite_master \
             WHERE type = 'index' AND name = 'people_nickname_idx'",
        )
        .await
        .expect("read dependent index after rename");
    let dependent_index = dependent_index[0][0]
        .as_deref()
        .expect("dependent index SQL survives");
    assert!(
        (dependent_index.contains("(handle)") || dependent_index.contains("(\"handle\")"))
            && !dependent_index.contains("(nickname)")
            && !dependent_index.contains("(\"nickname\")"),
        "SQLite's own rename parser updates captured dependent index SQL: {dependent_index}"
    );

    // The journal records the REBUILD migration as Completed — the proof it ran via
    // `rebuild_one` and NOT `run_online` (run_online journals the PG E1..C2
    // expand sub-steps, a wholly different version set; here the only journaled
    // online-rename version is the single rebuild migration's).
    let applied = be.applied(&exec_cfg()).await.expect("journal");
    assert!(
        applied.iter().any(|e| e.version == rebuild_version
            && matches!(e.phase, zero_migrate::apply::journal::Phase::Completed)),
        "the rebuild migration is journaled completed (rebuild_one path)"
    );
    // And NO PG expand-contract sub-step ever journaled (the run_online path was
    // never taken). `version` is a UUIDv7 (MigrationId::generate), NOT the human
    // "expand_*"/"contract_*" name — so a name-prefix check is vacuous. Instead
    // prove it by VERSION-SET DIFFERENCE: the rename added EXACTLY the one rebuild
    // version to the journal. run_online would journal the E1..C2 expand sub-steps
    // as *additional, distinct* versions; their absence is the load-bearing proof.
    let after: std::collections::BTreeSet<String> = applied
        .iter()
        .filter(|e| matches!(e.phase, zero_migrate::apply::journal::Phase::Completed))
        .map(|e| e.version.as_str().to_string())
        .collect();
    let added: std::collections::BTreeSet<String> = after.difference(&before).cloned().collect();
    assert_eq!(
        added,
        std::collections::BTreeSet::from([rebuild_version.clone()]),
        "the SQLite rename adds EXACTLY the one rebuild version to the journal — \
         no extra expand/contract sub-step versions (run_online was never taken)"
    );
}

// Neutral-type translation on the SQLite leg: a renameColumn whose neutral
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
        let author = IrAuthor::new(
            PROJECT,
            APP,
            SqlDialect::Sqlite,
            &support::confined_charter(),
        );
        let engine = MigrationEngine::new();
        let ir = MigrationIr {
            ir_version: 1,
            name: "create_events".into(),
            owner_app: APP.into(),
            ops: vec![Op::CreateTable {
                name: "events".into(),
                columns: vec![zero_migrate::model::ir::IrColumn {
                    name: "count".into(),
                    ty: ColType::Int,
                    nullable: Some(false),
                    default: None,
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                }],
                primary_key: None,
                constraints: vec![],
                indexes: vec![],

                partition_by: None,

                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("create");
        engine
            .apply_plan(
                &steps,
                Approval::None,
                &be,
                &exec_cfg(),
                "deploy",
                LockMode::Acquire,
            )
            .await
            .expect("apply create");
    }

    let live = live_schema_for(&v1);
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
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

// IR-vs-live type reconciliation is SYMMETRIC across the two
// legs: the SQLite leg must reject a wrong IR `ty` IDENTICALLY to the PG leg
// (`RenameTypeMismatch`), not silently ignore the IR type and use the live type.
// Pre-fix the SQLite leg carried the live `data_type` across UNCHANGED and renamed
// only the SDK field KEY, so the neutral `ColType` was decorative — a wrong `ty`
// (here `Int` over a live `string`/text column) lowered with NO rejection. The
// reconciliation now runs BEFORE the dialect dispatch, so both legs fail closed on
// the same mismatch (proving the two cannot diverge-detect a wrong `ty`).
#[test]
fn renamecolumn_sqlite_rejects_ir_type_disagreeing_with_live_column() {
    // v1: people(nickname:string) — the live column is text-affinity.
    let v1 = vec![descriptor("people", "nickname", "string")];
    let live = live_schema_for(&v1);
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    // The IR claims the renamed column is `Int` — disagreeing with the live text type.
    let ir = rename_ir("people", "nickname", "handle", ColType::Int);
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("the SQLite leg must reject a wrong IR type identically to PG");
    match err {
        IrLowerError::RenameTypeMismatch {
            table, from, to, ..
        } => {
            assert_eq!(table, "people");
            assert_eq!(from, "nickname");
            assert_eq!(to, "handle");
        }
        other => panic!("expected RenameTypeMismatch on the SQLite leg, got: {other}"),
    }
}

// Cross-app guard on the SQLite rebuild leg. The rebuild
// routes through the declarative differ, whose `enforce_ownership` refuses a
// structural change to a FOREIGN table. Pre-fix `sqlite_rename_rebuild` fabricated
// BOTH ownership maps as the deploying app, so app B could silently rebuild app
// A's table. Post-fix the REAL introspected owner is carried in
// `LiveSchema::table_ownership`; a rename by a non-owner is rejected. Here the
// table is owned by `app_other` but the IrAuthor deploys as `APP`.
#[test]
fn renamecolumn_sqlite_rejects_cross_app_rename() {
    let v1 = vec![descriptor("people", "nickname", "string")];
    // Build the live facts, then OVERWRITE the owner to a different app.
    let mut live = live_schema_for(&v1);
    live.table_ownership
        .insert("people".into(), "app_other".into());

    // The IrAuthor deploys as `APP` (≠ app_other) — a non-owner rename.
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let ir = rename_ir("people", "nickname", "handle", ColType::Text);
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a non-owner SQLite rename must be refused by the cross-app guard");
    // The differ's NotTableOwner surfaces as a RenameLower (the bridge error). The
    // message names the foreign owner.
    match err {
        IrLowerError::RenameLower(msg) => {
            assert!(
                msg.contains("app_other") || msg.to_lowercase().contains("owner"),
                "the refusal must reference the foreign ownership: {msg}"
            );
        }
        other => panic!("expected RenameLower (cross-app refusal), got: {other}"),
    }
}

// Fail-closed: a SQLite renameColumn whose table's full live structure is
// NOT in the LiveSchema cannot lower — it refuses rather than emit a rebuild from
// a partial view of the table. With the IR-vs-live type reconciliation now gating
// the lowering BEFORE the dialect dispatch, the absence of the live `from` column
// trips that gate first (`RenameNeedsLiveColumn`): a strictly-earlier, equally
// fail-closed refusal that ALSO needs the live column. Either refusal is correct
// (both are fail-closed and emit NO rebuild); the type-reconciliation gate is the
// outermost, so it is the one observed. The deeper `SqliteRenameNeedsLiveTable`
// arm still guards the case where the live `from` column type IS known but the
// full rebuild shape (sqlite_schemas) is not — exercised by
// `renamecolumn_sqlite_fails_closed_with_column_but_no_sqlite_schema`.
#[test]
fn renamecolumn_sqlite_fails_closed_without_live_table_structure() {
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let ir = rename_ir("ghost", "a", "b", ColType::Text);
    // LiveSchema knows the table NAME but not its structure (table_snapshots /
    // sqlite_schemas empty) — there is no live `from` column to reconcile against.
    let mut live = LiveSchema::default();
    live.tables.insert("ghost".into());
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a SQLite rename with no live table structure must fail closed");
    match err {
        IrLowerError::RenameNeedsLiveColumn(t, c) => {
            assert_eq!(t, "ghost");
            assert_eq!(c, "a");
        }
        other => {
            panic!("expected RenameNeedsLiveColumn (the outermost fail-closed gate), got: {other}")
        }
    }
}

// Fail-closed (deeper arm): the live `from` column TYPE is known (so the
// type reconciliation passes), but the full rebuild shape — the live SDK schema
// `Value` in `sqlite_schemas` — is absent. The SQLite leg then refuses with
// `SqliteRenameNeedsLiveTable` rather than emit a rebuild from a partial view.
// This keeps the rebuild-needs-whole-shape guard exercised after the type gate.
#[test]
fn renamecolumn_sqlite_fails_closed_with_column_but_no_sqlite_schema() {
    use zero_migrate::{ColumnSnapshot, TableSnapshot};
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let ir = rename_ir("ghost", "a", "b", ColType::Text);
    // Carry the live `from` column TYPE (so the type gate passes — text == text),
    // but DO NOT populate `sqlite_schemas` (the rebuild's SDK Value is missing).
    let a_type = {
        // Derive the live `data_type` for a text column the SAME way the builder
        // does, so the reconciliation passes (live == IR-derived).
        let v1 = vec![descriptor("ghost", "a", "string")];
        live_schema_for(&v1).table_snapshots["ghost"].columns[0]
            .data_type
            .clone()
    };
    let mut live = LiveSchema::default();
    live.tables.insert("ghost".into());
    live.table_snapshots.insert(
        "ghost".into(),
        TableSnapshot {
            columns: vec![ColumnSnapshot {
                name: "a".into(),
                data_type: a_type,
                nullable: true,
                default: None,
                ddl_type_override: None,
                inline_checks: Vec::new(),
                generated: None,
                identity: None,
                sqlite_rowid: false,
                value_format: None,
                catalog_uuid_format_check: false,
                id_default: None,
                mysql_default_generated: None,
                case_sensitive: None,
                collation: None,
                mysql_text_storage: None,
                mysql_physical_type: None,
                encryption_sentinel: None,
                comment_sentinel: None,
                comment: None,
            }],
            indexes: vec![],
            constraints: vec![],
            runtime_options: Default::default(),

            partition_by: None,

            comment: None,
            stored_create_sql: None,
        },
    );
    let err = author
        .lower_steps(&ir, &live)
        .expect_err("a SQLite rename with the column type but no SDK schema must fail closed");
    match err {
        IrLowerError::SqliteRenameNeedsLiveTable(t) => assert_eq!(t, "ghost"),
        other => panic!("expected SqliteRenameNeedsLiveTable, got: {other}"),
    }
}

/// A two-text-field-collection descriptor (`<a>: string, <b>: string`, required) —
/// for the rename-to-existing-column collision case (both `from` and `to` are live).
fn descriptor2(table: &str, a: &str, b: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: a.into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: b.into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// A child table whose `parent_id` FK targets a different table in the same
/// project, plus an ordinary text column that can be renamed independently.
fn referencing_descriptor(table: &str, parent: &str) -> CollectionDescriptor {
    CollectionDescriptor {
        name: table.into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "parent_id".into(),
                ty: "ref".into(),
                required: true,
                references: Some(parent.into()),
                ..Default::default()
            },
            FieldDescriptor {
                name: "nickname".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

// A scoped rename diff contains only the rebuilt child table, but its FK target
// may be another table already present in the complete live-schema universe. The
// lowerer must validate against `LiveSchema::tables`, then retain the FK in the
// rebuilt CREATE rather than treating that external target as dangling.
#[test]
fn renamecolumn_sqlite_retains_fk_to_another_known_live_table() {
    let parent = descriptor("departments", "name", "string");
    let child = referencing_descriptor("employees", "departments");
    let live = live_schema_for(&[parent, child]);
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );

    let steps = author
        .lower_steps(
            &rename_ir("employees", "nickname", "handle", ColType::Text),
            &live,
        )
        .expect("the known external FK target permits the scoped rename diff");
    let create = match &steps[..] {
        [PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))] => {
            &rebuild.spec.new_table_create
        }
        other => panic!("expected one SQLite rebuild, got {other:?}"),
    };

    assert!(
        create.contains("REFERENCES \"departments\" (id)"),
        "the rebuild must retain the child FK to the known live parent: {create}"
    );
}

// The complete live-table set is the validation authority for FK targets outside
// the one-table rename diff. Removing the parent from that set must still fail
// closed, even though the test fixture retains unrelated cached parent details in
// its other LiveSchema maps.
#[test]
fn renamecolumn_sqlite_rejects_fk_to_table_missing_from_live_table_set() {
    let parent = descriptor("departments", "name", "string");
    let child = referencing_descriptor("employees", "departments");
    let mut live = live_schema_for(&[parent, child]);
    assert!(live.tables.remove("departments"));
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );

    let error = author
        .lower_steps(
            &rename_ir("employees", "nickname", "handle", ColType::Text),
            &live,
        )
        .expect_err("a scoped rename must reject a genuinely missing FK target");
    match error {
        IrLowerError::RenameLower(message) => {
            assert!(
                message.contains("employees")
                    && message.contains("departments")
                    && message.contains("no app"),
                "the dangling-FK refusal must identify the child and target: {message}"
            );
        }
        other => panic!("expected RenameLower for a dangling FK target, got: {other}"),
    }
}

// Rename-to-EXISTING-column collision (SQLite leg). A `renameColumn` whose
// `to` equals a column that ALREADY exists on the live table must fail closed at the
// LOWER gate (`RenameLower`), NOT silently OVERWRITE the existing `to` field def when
// the rebuild renames the `from` key onto it (a data-loss-class silent mis-build).
// The live table carries BOTH `nickname` (the from) and `handle` (the to).
#[test]
fn renamecolumn_sqlite_rejects_rename_to_existing_column() {
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    // Live `people(nickname, handle)` — both real columns + SDK schema entries.
    let live = live_schema_for(&[descriptor2("people", "nickname", "handle")]);
    // Rename nickname → handle, but `handle` ALREADY exists.
    let ir = rename_ir("people", "nickname", "handle", ColType::Text);
    let err = author.lower_steps(&ir, &live).expect_err(
        "a SQLite rename whose `to` collides with an existing live column must fail closed",
    );
    match err {
        IrLowerError::RenameLower(msg) => {
            assert!(
                msg.contains("handle") && msg.contains("already exists"),
                "the collision error must name the offending `to` column: {msg}"
            );
        }
        other => panic!("expected RenameLower (to-collision), got: {other}"),
    }
}

// TWO renameColumn ops on ONE table in ONE migration are refused at lower.
//
// SQLite reconciles a rename with the 12-step table rebuild, whose CREATE comes
// from `TableSnapshot::stored_create_sql` - the verbatim `sqlite_master.sql` text.
// It is byte-faithful on purpose, so the rebuild can hand the identifier rewrite
// to SQLite's own `ALTER TABLE ... RENAME COLUMN` parser and let CHECKs, generated
// expressions, indexes and triggers follow untouched. A second rebuild in the same
// envelope would still be built from the first one's text, and the engine cannot
// update it without exactly the lossy rewrite that design avoids.
//
// MEASURED before this refusal existed: the second rebuild kept the pre-rename
// CREATE while its copy list had moved on, and SQLite rejected the mismatch with
// `table people__zero_migrate_rebuild has no column named handle`. That failed
// mid-apply and named an intermediate table; refusing at lower names the repair.
//
// The control below is what keeps the refusal from being read as "two ops on one
// table are refused" - renaming columns on two DIFFERENT tables in one migration
// still lowers, because each table's stored text is still its own.
#[compio::test]
async fn two_renames_of_one_table_in_one_migration_are_refused_on_sqlite() {
    let p = paths("sqlite_two_renames");
    let be = backend(&p);

    let v1 = vec![descriptor2("people", "nickname", "city")];
    first_deploy(&be, &v1).await;

    let mut live = live_schema_for(&v1);
    let catalog = be
        .snapshot_schema_sqlite()
        .await
        .expect("introspect the real pre-rename table");
    live.tables = catalog.tables.keys().cloned().collect();
    live.table_snapshots = catalog.tables;

    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let mut ir = rename_ir("people", "nickname", "handle", ColType::Text);
    ir.ops.push(Op::RenameColumn {
        table: "people".into(),
        from: "city".into(),
        to: "town".into(),
        ty: ColType::Text,
        schema: None,
        existence_guard: None,
    });

    let error = author
        .lower_steps(&ir, &live)
        .expect_err("two renames of one table must be refused before anything runs");
    let message = error.to_string();
    assert!(
        matches!(error, IrLowerError::SqliteRepeatRenameTarget(ref t) if t == "people"),
        "the refusal is the dedicated variant naming the table: {message}"
    );
    assert!(
        message.contains("Split the renames across separate migrations"),
        "and it names the repair, which the apply-time failure never did: {message}"
    );

    // The control. Two renames of DIFFERENT tables in one migration still lower,
    // so the refusal is about one table's stored text being consumed twice, not
    // about a migration carrying more than one rename.
    let v2 = vec![
        descriptor2("people", "nickname", "city"),
        descriptor2("places", "label", "region"),
    ];
    let mut two_tables = rename_ir("people", "nickname", "handle", ColType::Text);
    two_tables.ops.push(Op::RenameColumn {
        table: "places".into(),
        from: "label".into(),
        to: "title".into(),
        ty: ColType::Text,
        schema: None,
        existence_guard: None,
    });
    author
        .lower_steps(&two_tables, &live_schema_for(&v2))
        .expect("renames on two different tables still lower");
}
