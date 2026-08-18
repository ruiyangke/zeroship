//! Rolling back a view REPLACE, proven against a real `SQLite` file.
//!
//! A replace changes an existing view's body. Its inverse is therefore "put the previous
//! body back", not "remove the view" - the view predates the migration being undone.
//!
//! `crates/zero-migrate/src/render/lower.rs` used to synthesise the `createView`
//! statement's `down` as `DROP VIEW IF EXISTS` unconditionally, with `replace` reaching
//! the `up` prefix and the replace prelude but never the `down`. That was harmless while
//! the fold refused a replace against a view an applied migration had created: the arm
//! could only be reached when the create had brought the view into being, and dropping
//! really was the inverse. Commit a1fe1047 made the across-deploys replace applicable,
//! which made the arm reachable in the one case it gets wrong - a rollback that reported
//! success while deleting a view the migration never created.
//!
//! TWO ARMS, and the pair is the point:
//!
//!   - what SHIPS: a replace carries no `down`, so a rollback is REFUSED. Safety, not
//!     capability - strictly better than destroying, strictly worse than restoring.
//!   - what is AIMED AT: a replace restores the previous body. Ignored, because reaching
//!     it means widening `ViewStatement.down` past a single statement.
//!
//! Keeping both means the shipped behaviour is pinned AND the gap it leaves is legible,
//! rather than the refusal reading as the finished answer.
//!
//! Everything is read from `sqlite_master`, never from the plan the engine meant to run.

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

/// The END STATE this area is aimed at: a replace is reversible and restores the previous
/// body. NOT what ships today - today a replace is refused as irreversible, which the
/// sibling test below pins.
///
/// Kept ignored rather than deleted because it states the contract a restoring fix has to
/// meet, and because the measurement behind it was the expensive part. Reaching it means
/// widening `ViewStatement.down` past one statement: SQLite has no
/// `CREATE OR REPLACE VIEW` - `view_create_prefix` ignores the flag
/// (`render/renderer.rs:444-452`) and its replace is carried by a prelude that drops
/// first - so a faithful restore is DROP + CREATE there and one statement on PostgreSQL.
#[ignore = "aspirational: a restoring fix would make this pass; today the replace is refused (#209)"]
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

/// What ships: a replace is IRREVERSIBLE, so the rollback is refused rather than run.
///
/// This is the safety half of the pair above. Refusing is strictly better than the
/// behaviour it replaces - a rollback that reported success while deleting a view the
/// migration never created - and it is strictly worse than restoring, which is why the
/// aspirational arm stays in the file.
///
/// The refusal is one an operator sees. `apply/executor.rs:2563` returns
/// `RollbackError::Irreversible` naming the version, and only a `force` carrying an
/// explicit `backup_acknowledged` proceeds past it, recording the version under
/// `skipped_irreversible` rather than passing silently.
#[compio::test]
async fn a_replace_is_refused_as_irreversible_rather_than_dropping_the_view() {
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
        "the view must exist before the replace"
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
    assert_ne!(
        view_sql(&be, VIEW).await,
        original,
        "the replace must change the stored body, or this proves nothing about undoing one"
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
    .expect_err("a replace carries no faithful inverse, so the rollback must refuse");

    assert!(
        matches!(&error, RollbackError::Irreversible { .. }),
        "the refusal must name irreversibility rather than failing for some other reason: {error:?}"
    );

    // The point of refusing: the view is untouched. A rollback that ran would have
    // removed it.
    assert!(
        view_exists(&be, VIEW).await,
        "a refused rollback must leave the view standing"
    );
}
