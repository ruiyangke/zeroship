//! Rolling back a view REPLACE, proven against a real `SQLite` file.
//!
//! A replace changes an existing view's body. Its inverse is therefore "put the previous
//! body back", not "remove the view" - the view predates the migration being undone.
//!
//! `crates/zero-migrate/src/render/lower.rs` synthesises the `createView` statement's
//! `down` as `DROP VIEW IF EXISTS` unconditionally; `replace` reaches the `up` prefix and
//! the replace prelude but never the `down`. That was harmless while the fold refused a
//! replace against a view an applied migration had created, because the arm could only be
//! reached when the create had brought the view into being and dropping really was the
//! inverse. Commit a1fe1047 made the across-deploys replace applicable, which made this
//! arm reachable in the case it gets wrong.
//!
//! This asserts the CORRECT behaviour rather than the current one, so the failure names
//! the real state of the database instead of pinning a defect in place. Everything is
//! read from `sqlite_master`, never from the plan the engine meant to run.

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::executor::{rollback, LockMode, RollbackRequest, RollbackTarget};
use zero_migrate::model::ir::Op;
use zero_migrate::model::migration::Migration;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "replace_view_rollback";
const APP: &str = "app_replace_view_rollback";
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

/// The original body: one column.
fn create_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "create_active_users",
        "ops": [
            {
                "op": "createTable",
                "name": "users",
                "columns": [{"name": "email", "type": "text", "nullable": false}]
            },
            {
                "op": "createView",
                "name": VIEW,
                "query": {"kind": "structured", "select": {
                    "from": {"name": "users"},
                    "projection": [
                        {"kind": "colRef", "name": "id"}
                    ],
                    "joins": [],
                    "groupBy": []
                }}
            }
        ]
    })
    .to_string()
}

/// The replacing body, which APPENDS a column. SQLite would also accept a narrower
/// projection, but PostgreSQL refuses to drop columns from a view - appending is the one
/// replace shape both engines allow, so this fixture stays comparable across dialects
/// rather than exercising a shape only one of them permits.
fn replace_doc() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "replace_active_users",
        "ops": [{
            "op": "createView",
            "name": VIEW,
            "replace": true,
            "query": {"kind": "structured", "select": {
                "from": {"name": "users"},
                "projection": [
                    {"kind": "colRef", "name": "id"},
                    {"kind": "colRef", "name": "email"}
                ],
                "joins": [],
                "groupBy": []
            }}
        }]
    })
    .to_string()
}

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

/// MEASURED FAILING. Ignored so the gate stays honest rather than red, NOT because the
/// behaviour is acceptable: rolling back this migration removes a view the migration
/// never created.
///
/// It is carried here rather than deleted because the measurement was the expensive part
/// and the assertion states the contract the fix has to meet. Un-ignore it with the fix.
///
/// The fix is not a one-liner, which is why this is ignored rather than repaired in the
/// same commit. `ViewStatement.down` holds ONE statement, and SQLite has no
/// `CREATE OR REPLACE VIEW` - `view_create_prefix` ignores the flag
/// (`render/renderer.rs:444-452`) and the replace is carried by a separate prelude that
/// drops first. So restoring a prior body needs either a `down` that can hold several
/// statements or a per-dialect split, and that is a design decision rather than a patch.
#[ignore = "measured failing: rolling back a view replace drops the view (#209)"]
#[compio::test]
async fn rolling_back_a_replace_restores_the_previous_body() {
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

    let original = view_sql(&be, VIEW).await;
    assert!(
        !original.is_empty(),
        "the view must exist with a body before it is replaced, or the rest proves nothing"
    );

    let registry: BTreeMap<String, String> = [("users".to_string(), APP.to_string())]
        .into_iter()
        .collect();
    migrations.extend(
        apply_doc(
            &be,
            &replace_doc(),
            &registry,
            &mut history,
            Approval::Approved,
        )
        .await,
    );

    let replaced = view_sql(&be, VIEW).await;
    assert_ne!(
        replaced, original,
        "the replace must actually change the stored body, or the rollback has nothing to undo"
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
    .expect("rolling back the replace must succeed");

    // The view predates the migration being undone, so undoing it must leave the view
    // standing. A rollback that removes it destroys an object the migration never
    // created.
    assert!(
        view_exists(&be, VIEW).await,
        "rolling back a replace must not drop a view that existed before the migration"
    );
    assert_eq!(
        view_sql(&be, VIEW).await,
        original,
        "rolling back a replace must restore the body the view had before it"
    );
}
