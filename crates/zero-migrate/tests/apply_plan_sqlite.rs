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
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::{PlanStep, RenameStep};
use zero_migrate::{
    desired_snapshot, Approval, CollectionDescriptor, DeclarativeAuthor, ExecutorConfig,
    FieldDescriptor, MigrationBackend, MigrationEngine, RenameHint, SchemaSnapshot, SqliteBackend,
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
    runtime_options: Default::default(),
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
            zero_migrate::apply::executor::LockMode::Acquire,
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
                && matches!(e.phase, zero_migrate::apply::journal::Phase::Completed)),
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
            zero_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("re-run");
    assert!(
        out2.applied.applied.is_empty(),
        "re-running the rebuild applies nothing (net-applied-skip)"
    );
    assert_eq!(out2.applied.skipped, vec![rebuild_version]);
}

// ---------------------------------------------------------------------------
// REGRESSION (code-critic MED): apply_plan bootstraps the journal up front.
//
// A standalone plan whose FIRST step is `OnlineRename(SqliteRebuild)`, applied
// against a FRESH SQLite file with NO `_mig` journal, made its first journal
// touch a READ (`journal_sql::applied` SELECT on a non-existent
// `_mig.schema_migrations`, via the rebuild arm's net-applied-skip lookup) →
// "no such table". The shipped declarative path always bootstrapped the journal
// before any read, but `apply_plan` is public API consumed with this net-new
// rebuild-first shape.
//
// The v1 schema is built (with its full system columns) on one backend, then the
// rebuild-first plan is driven on a SECOND backend opened over the SAME app file
// but a FRESH (empty) journal file — faithfully reproducing "existing schema, no
// journal yet". Pre-fix: errors on the journal read (`no such table`). Post-fix:
// the up-front `ensure_journal` bootstraps `_mig` and the rebuild applies.
#[compio::test]
async fn rebuild_first_plan_against_fresh_journal_bootstraps_it() {
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

    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join("zs-app.sqlite");

    // 1) Build the v1 `people` table (with its full descriptor-generated system
    //    columns) on a FIRST backend; this also bootstraps that backend's journal.
    let journal_a = dir.path().join("zs-app.migrations-a.sqlite");
    {
        let be_a = SqliteBackend::open(&app, &journal_a).expect("open backend A");
        apply_first_deploy(&be_a, &v1).await;
        be_a.actor().set_mode(Mode::EngineJournal).await.expect("mode");
        be_a.actor()
            .exec("INSERT INTO main.people (id, nickname) VALUES ('p1', 'ada')")
            .await
            .expect("seed row");
    }

    // 2) Build the SqliteRebuild via the differ (pure — no DB write).
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
    let rebuild = plan.rebuilds.into_iter().next().unwrap();
    let rebuild_version = rebuild.migration.version.as_str().to_string();

    // 3) Open a SECOND backend over the SAME app file but a FRESH journal file:
    //    the schema is fully present, the `_mig` journal is empty/unbootstrapped.
    let journal_b = dir.path().join("zs-app.migrations-b.sqlite");
    let be = SqliteBackend::open(&app, &journal_b).expect("open backend B (fresh journal)");

    // The FIRST (and only) step is the SqliteRebuild; the `_mig` journal does not
    // exist yet — the rebuild arm's net-applied-skip lookup reads it first.
    let steps = vec![PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))];
    let engine = MigrationEngine::new();
    let out = engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &be,
            &exec_cfg(),
            "deployer",
            zero_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("a rebuild-first plan against a fresh journal must bootstrap it up front");

    assert!(
        out.applied.applied.contains(&rebuild_version),
        "apply_plan journaled the rebuild migration's version"
    );
    let vals = be
        .actor()
        .query("SELECT handle FROM main.people ORDER BY id")
        .await
        .expect("read handle");
    assert_eq!(
        vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec!["ada"],
        "the renamed column carries the data through the rebuild"
    );
}

// ---------------------------------------------------------------------------
// PR9a (§2.0.3 / Deliverable 7): the SQLite rebuild rename has NO pending-contract
// partition, so the cross-deploy interlock can NEVER false-gate a SQLite deploy.
//
// After a SQLite rebuild rename completes, the backend reports an EMPTY outstanding
// pending-contract set (a SQLite rename never opens an obligation), and a
// FOLLOW-ON deploy that touches the SAME just-renamed table — which on the PG leg
// would be refused while a contract is pending — applies cleanly. This pins that
// the interlock is structurally PG-only.
#[compio::test]
async fn sqlite_rename_opens_no_obligation_and_never_gates_a_follow_on_deploy() {
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

    let p = paths("sqlite_no_gate");
    let be = backend(&p);
    apply_first_deploy(&be, &v1).await;

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
    let rebuild = plan.rebuilds.into_iter().next().unwrap();
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &[PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild))],
            Approval::Approved,
            &be,
            &exec_cfg(),
            "deployer",
            zero_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("sqlite rebuild rename applies");

    // Deliverable 7: SQLite structurally has no pending-contract capability.
    assert!(
        be.pending_contracts().is_none(),
        "a SQLite rebuild rename has NO pending-contract partition"
    );

    // A follow-on deploy that TOUCHES the just-renamed `people` table is NOT gated
    // (on PG this would be refused while a contract is pending; on SQLite there is
    // no pending partition, so it proceeds — the interlock is structurally PG-only).
    let add = zero_migrate::model::migration::Migration {
        version: zero_migrate::model::migration::MigrationId::generate(),
        name: "add_people_label".into(),
        up: "ALTER TABLE people ADD COLUMN label text".into(),
        down: None,
        checksum: zero_migrate::model::migration::Checksum::of(
            &zero_migrate::model::migration::ChecksumInput {
                up: "addlabel",
                down: None,
                flags: &zero_migrate::model::migration::MigrationFlags::default(),
                owner_app: APP,
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            },
        ),
        flags: zero_migrate::model::migration::MigrationFlags::default(),
        owner_app: APP.into(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        existence_guard: None,
    };
    engine
        .apply_plan_with_touched(
            &[PlanStep::Ddl(add)],
            &["people".to_string()],
            Approval::None,
            &be,
            &exec_cfg(),
            "deployer",
            zero_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect("a follow-on touch of the renamed table is NEVER gated on SQLite");
}
