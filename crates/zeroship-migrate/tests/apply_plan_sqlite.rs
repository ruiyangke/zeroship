//! PR0 — faithful e2e for the single shared `apply_plan`'s SQLite leg of the
//! dual-EXECUTION rename dispatch (`op.*` DSL §2.6.2), against REAL temp-file
//! SQLite.
//!
//! Test (5): a `PlanStep::OnlineRename(RenameStep::SqliteRebuild(_))` driven
//! THROUGH `apply_plan` executes via `MigrationBackend::rebuild_one` (the 12-step
//! offline rebuild), NOT `run_online` (SQLite has no online capability). A seeded
//! row survives the rename, the old column is gone, and the journal records the
//! rebuild migration's version — proving `apply_plan` routes the SQLite variant to
//! `rebuild_one`, with no `pending_contract` partition (a SQLite rebuild is one
//! atomic offline step).

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::backend_sqlite::Mode;
use zeroship_migrate::plan::{PlanStep, RenameStep};
use zeroship_migrate::{
    desired_snapshot, Approval, CollectionDescriptor, DeclarativeAuthor, ExecutorConfig,
    FieldDescriptor, MigrationBackend, MigrationEngine, RenameHint, SchemaSnapshot, SqliteBackend,
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

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT)
}

fn live_from(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_snapshot(PROJECT, descs).expect("first-deploy desired_snapshot");
    let ownership: HashMap<String, String> = d
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    (d.snapshot, ownership)
}

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

#[compio::test]
async fn sqlite_online_rename_executes_via_rebuild_one_through_apply_plan() {
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

    let p = paths("apply_plan_rename");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.people (id, nickname) VALUES ('p1', 'ada'), ('p2', 'grace')")
        .await
        .expect("seed rows");

    // Produce the SqliteRebuild via the differ + a rename hint (one rebuild, no
    // PG expand-contract on SQLite).
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
    assert!(plan.renames.is_empty(), "no PG expand-contract on SQLite");
    let rebuild = plan.rebuilds.into_iter().next().unwrap();
    let rebuild_version = rebuild.migration.version.as_str().to_string();

    // Drive it THROUGH the single shared apply_plan as a RenameStep::SqliteRebuild.
    // (Clone so the same rebuild — same version — can be replayed for the
    // idempotency check; the SQLite rebuild version is a freshly-minted UUIDv7, so
    // a re-diff would mint a different one; net-applied-skip keys on the SAME id.)
    let steps = vec![PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild.clone()))];
    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(
            &steps,
            Approval::Approved, // a rebuild on a populated table is destructive
            &be,
            &exec_cfg(),
            "deployer",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("apply_plan drives the SQLite rebuild");

    // No PG online partition: a SQLite rebuild is one atomic offline step.
    assert!(
        out.pending_contract.is_empty(),
        "the SQLite rebuild leg has NO pending_contract partition (§2.6.2)"
    );
    assert!(
        out.applied.applied.contains(&rebuild_version),
        "apply_plan journaled the rebuild migration's version"
    );

    // The data followed the rename: the renamed `handle` holds the old values.
    let vals = be
        .actor()
        .query("SELECT handle FROM main.people ORDER BY id")
        .await
        .expect("read handle");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["ada", "grace"],
        "the renamed column carries the data through apply_plan's rebuild dispatch"
    );
    // The old column is gone.
    let info = be
        .actor()
        .query("PRAGMA main.table_info(people)")
        .await
        .expect("table_info");
    assert!(
        info.iter().all(|r| r[1].as_deref() != Some("nickname")),
        "the old column name is gone after the rebuild rename"
    );

    // The journal records the rebuild's version (via rebuild_one, not run_online).
    let applied = be.applied(&exec_cfg()).await.expect("read journal");
    assert!(
        applied
            .iter()
            .any(|e| e.version == rebuild_version
                && matches!(e.phase, zeroship_migrate::journal::Phase::Completed)),
        "the rebuild migration's version is journaled completed"
    );

    // Idempotent re-run: replay the SAME rebuild (same version) → net-applied-skip,
    // no double rebuild.
    let steps2 = vec![PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))];
    let out2 = engine
        .apply_plan(
            &steps2,
            Approval::Approved,
            &be,
            &exec_cfg(),
            "deployer",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await
        .expect("re-run");
    assert!(
        out2.applied.applied.is_empty(),
        "re-running the rebuild applies nothing (net-applied-skip)"
    );
    assert_eq!(out2.applied.skipped, vec![rebuild_version]);
}
