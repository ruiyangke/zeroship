//! Rolling back a dropped view restores it, proven against a real `SQLite` file.
//!
//! `Op::DropView` lowers with no `down`, so the rollback planner has nothing to
//! run and the view stays dropped. The inverse is recoverable without touching
//! the database: `Op::CreateView` carries the view body, so the migration
//! history already holds every byte the `CREATE VIEW` needs.
//!
//! The whole path is real. The IR goes through `load_ir_document` + `lower_plan`
//! + `MigrationEngine::apply_plan` onto a temp-file backend, and every assertion
//! reads `sqlite_master` rather than the plan the engine intended to run.

use crate::support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::executor::{
    rollback, LockMode, RollbackError, RollbackRequest, RollbackTarget,
};
use zero_migrate::model::ir::Op;
use zero_migrate::model::migration::Migration;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "drop_view_rollback";
const APP: &str = "app_drop_view_rollback";
const VIEW: &str = "active_users";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths() -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join("zs-app.sqlite");
    let journal = dir.path().join("zs-app.migrations.sqlite");
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT))
}

/// Does the view PHYSICALLY exist? Reads `sqlite_master`, not the engine's plan.
async fn view_exists(be: &SqliteBackend, name: &str) -> bool {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_master WHERE type='view' AND name='{name}'"
        ))
        .await
        .expect("sqlite_master view");
    !rows.is_empty()
}

/// The view body `sqlite_master` stored, so a restored view can be compared
/// against the one that was dropped rather than merely counted.
async fn view_sql(be: &SqliteBackend, name: &str) -> String {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT sql FROM main.sqlite_master WHERE type='view' AND name='{name}'"
        ))
        .await
        .expect("sqlite_master view sql");
    rows.first()
        .and_then(|row| row.first())
        .cloned()
        .flatten()
        .unwrap_or_default()
}

/// The structured body both the create and the expected restore render from.
fn active_users_view_op() -> Op {
    let select = serde_json::json!({
        "from": {"name": "users"},
        "projection": [
            {"kind": "colRef", "name": "id"},
            {"kind": "colRef", "name": "email"}
        ],
        "joins": [],
        "groupBy": []
    });
    serde_json::from_value(serde_json::json!({
        "op": "createView",
        "name": VIEW,
        "query": {"kind": "structured", "select": select}
    }))
    .expect("createView op parses")
}

fn create_doc() -> String {
    let mut ops = vec![serde_json::json!({
        "op": "createTable",
        "name": "users",
        "columns": [
            {"name": "email", "type": "text", "nullable": false}
        ]
    })];
    ops.push(serde_json::to_value(active_users_view_op()).expect("view op serializes"));
    serde_json::json!({
        "ir_version": 1,
        "name": "create_active_users",
        "ops": ops
    })
    .to_string()
}

fn drop_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "drop_active_users",
        "ops": [{"op": "dropView", "name": VIEW}]
    })
    .to_string()
}

fn guarded_drop_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "drop_active_users_if_present",
        "ops": [{"op": "dropView", "name": VIEW, "existenceGuard": "ifExists"}]
    })
    .to_string()
}

/// Apply one IR doc through the real lower + apply path, returning the DDL
/// migrations it produced so the caller can hand them to the rollback planner.
///
/// `history` is the ordered op stream of every migration applied so far. It is
/// folded into the live schema exactly the way the deploy path does it
/// (`refresh_historical_live`, engine.rs:390-392), because that fold is what
/// carries an earlier `createView`'s body forward to the `dropView` that undoes it.
/// Building the live schema from table names alone would leave the body behind and
/// prove nothing about the production path.
async fn apply_doc(
    be: &SqliteBackend,
    ir: &str,
    reg: &BTreeMap<String, String>,
    history: &mut Vec<Op>,
    approval: Approval,
) -> Vec<Migration> {
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let document = zero_migrate::model::load::load_ir_document(
        ir,
        APP,
        zero_migrate::model::validate::Dialect::Sqlite,
        reg,
        None,
    )
    .expect("load gate (sqlite)");
    let folded = zero_migrate::fold_ops(
        history,
        SqlDialect::Sqlite,
        PROJECT,
        &support::confined_charter(),
    )
    .expect("fold the applied history");
    let live = LiveSchema::from_catalog_snapshot(folded, APP);
    history.extend(document.ops.iter().cloned());
    let plan = author
        .lower_plan(&document, &live)
        .expect("lower the doc plan on SQLite");
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            approval,
            be,
            &exec_cfg(),
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("apply the authored plan on SQLite");
    plan.steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

#[compio::test]
async fn rolling_back_a_dropped_view_restores_it() {
    let p = paths();
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let mut history: Vec<Op> = Vec::new();

    let mut migrations = apply_doc(
        &be,
        &create_doc(),
        &BTreeMap::new(),
        &mut history,
        Approval::None,
    )
    .await;

    assert!(
        view_exists(&be, VIEW).await,
        "the view must exist before it is dropped, or the rest of this test proves nothing"
    );
    let before = view_sql(&be, VIEW).await;
    assert!(
        !before.is_empty(),
        "sqlite_master must carry the view body for the restore to be comparable"
    );

    let registry: BTreeMap<String, String> = [("users".to_string(), APP.to_string())]
        .into_iter()
        .collect();
    migrations.extend(
        apply_doc(
            &be,
            &drop_doc(),
            &registry,
            &mut history,
            Approval::Approved,
        )
        .await,
    );

    assert!(
        !view_exists(&be, VIEW).await,
        "the drop must actually remove the view"
    );

    let request = RollbackRequest::new(RollbackTarget::Steps(1));
    rollback(
        &be,
        &exec_cfg(),
        &request,
        &migrations,
        Approval::Approved,
        APP,
        &zero_migrate::SqliteDescriptorGuard::new(),
    )
    .await
    .expect("rolling back the dropped view must succeed");

    assert!(
        view_exists(&be, VIEW).await,
        "rolling back the drop must put the view back"
    );
    assert_eq!(
        view_sql(&be, VIEW).await,
        before,
        "the restored view must have the body it had before the drop"
    );
}

/// A guarded drop keeps no inverse, even though the body is sitting in the history.
///
/// `ifExists` can journal `completed` without running the `DROP` at all: the
/// existence-guard arm resolves `SatisfiedNoop`, skips the `up`, and still records
/// the version. Rolling that back with a `CREATE VIEW` would conjure an object that
/// never existed on this database, so the migration must stay irreversible and the
/// planner must refuse it.
#[compio::test]
async fn a_guarded_drop_keeps_no_inverse() {
    let p = paths();
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend");
    let mut history: Vec<Op> = Vec::new();

    let mut migrations = apply_doc(
        &be,
        &create_doc(),
        &BTreeMap::new(),
        &mut history,
        Approval::None,
    )
    .await;
    assert!(view_exists(&be, VIEW).await, "the view must exist first");

    let registry: BTreeMap<String, String> = [("users".to_string(), APP.to_string())]
        .into_iter()
        .collect();
    migrations.extend(
        apply_doc(
            &be,
            &guarded_drop_doc(),
            &registry,
            &mut history,
            Approval::Approved,
        )
        .await,
    );
    assert!(
        !view_exists(&be, VIEW).await,
        "the guarded drop still removes a view that is present"
    );

    let request = RollbackRequest::new(RollbackTarget::Steps(1));
    let error = rollback(
        &be,
        &exec_cfg(),
        &request,
        &migrations,
        Approval::Approved,
        APP,
        &zero_migrate::SqliteDescriptorGuard::new(),
    )
    .await
    .expect_err("a guarded drop must not be reversible");

    assert!(
        matches!(error, RollbackError::Irreversible { .. }),
        "expected the planner to refuse the guarded drop as irreversible, got {error:?}"
    );
    assert!(
        !view_exists(&be, VIEW).await,
        "the refused rollback must not have re-created the view"
    );
}
