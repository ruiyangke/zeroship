//! HIGH #1 (spec §6.0 / §10 PR0 test 8) — the GOLDEN-TRACE ORACLE, SQLite leg.
//!
//! The peer of `golden_trace_pg.rs` for the two SQLite-specific declarative paths
//! the re-point must preserve byte-for-byte:
//!
//! - (b) a SQLite RENAME-via-REBUILD (the offline 12-step `rebuild_one`, NOT
//!   expand-contract), driven through the live re-pointed `apply_declarative` vs.
//!   the independent pre-re-point oracle (`engine.apply` for the plain set +
//!   `backend.rebuild_one` for each rebuild, in historical order), and
//! - (g) the EMPTY-RENAMES FAIL-CLOSED guard: a plan that carries a PG
//!   expand-contract rename to a backend with NO online capability (SQLite) must
//!   fail closed, never silently drop the rename — asserted at the `apply_plan`
//!   level (the convergence point).
//!
//! Real temp-file SQLite throughout. The (b) capture is frozen into an immutable
//! committed fixture under `tests/golden-traces/`.

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::backend_sqlite::Mode;
use zeroship_migrate::journal::Phase;
use zeroship_migrate::{
    desired_snapshot, drift::SchemaSnapshot, Approval, ApprovalScope, CollectionDescriptor, DeclarativeApplyError,
    DeclarativeAuthor, DeclarativeDeployOutcome, DeclarativeDeployPlan, EngineError, ExecutorConfig,
    FieldDescriptor, GuardConfig, MigrationBackend, MigrationEngine, RenameHint, SqliteBackend,
};
use zeroship_schema::query::SqlDialect;

const PROJECT: &str = "prj_golden";
const APP: &str = "app_golden";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(app_id: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{app_id}.sqlite"));
    let journal = dir.path().join(format!("zs-{app_id}.migrations.sqlite"));
    Paths { _dir: dir, app, journal }
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

fn guard_cfg() -> GuardConfig {
    GuardConfig::confined_sqlite(PROJECT)
}

fn live_from(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_snapshot(PROJECT, descs).expect("desired_snapshot");
    let ownership = d.ownership.iter().map(|(t, a)| (t.clone(), a.clone())).collect();
    (d.snapshot, ownership)
}

/// The normalized SQLite capture: the journal trace (version|phase) in net order +
/// the `people` table column shape (PRAGMA table_info). Version ids are stable here
/// (PROJECT/APP are constants, not per-test tokens), so the bytes are frozen-stable.
async fn capture(be: &SqliteBackend) -> String {
    let mut out = String::new();
    out.push_str("# journal (#i|phase|kind), net order\n");
    let mut entries = be.applied_sqlite().await.expect("read journal");
    entries.sort_by(|a, b| a.version.cmp(&b.version));
    // The version ids are time-ordered UUIDv7 (per-run unique even for the same
    // PROJECT), so normalize them to a stable positional index — the trace SHAPE
    // (count, phase, journaled-kind, order) is what the fixture freezes.
    for (i, e) in entries.iter().enumerate() {
        out.push_str(&format!("#{i}|{:?}|{:?}\n", e.phase, e.kind));
    }
    out.push_str("# people table_info (cid name type notnull)\n");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let info = be
        .actor()
        .query("PRAGMA main.table_info(people)")
        .await
        .expect("table_info");
    for r in &info {
        out.push_str(&format!(
            "{} {} {} {}\n",
            r[0].clone().unwrap_or_default(),
            r[1].clone().unwrap_or_default(),
            r[2].clone().unwrap_or_default(),
            r[3].clone().unwrap_or_default(),
        ));
    }
    out
}

fn assert_frozen(name: &str, body: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden-traces");
    std::fs::create_dir_all(&dir).expect("golden-traces dir");
    let path = dir.join(format!("{name}.txt"));
    match std::fs::read_to_string(&path) {
        Ok(frozen) => assert_eq!(
            body, frozen,
            "golden-trace fixture `{name}` drift vs the frozen committed fixture at {}",
            path.display()
        ),
        Err(_) => {
            std::fs::write(&path, body).expect("write fresh fixture");
            panic!(
                "golden-trace fixture `{name}` was ABSENT — wrote a fresh capture to {}. \
                 Review + commit it; re-run to assert against the frozen copy.",
                path.display()
            );
        }
    }
}

/// First-deploy: apply each plain migration additively (the plan spine).
async fn apply_first_deploy(be: &SqliteBackend, desc: &[CollectionDescriptor]) {
    let desired = desired_snapshot(PROJECT, desc).expect("desired");
    let plan = sqlite_author()
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("first-deploy diff");
    for m in &plan.all_migrations() {
        be.apply_one_additive(m, "deployer")
            .await
            .unwrap_or_else(|e| panic!("first-deploy apply {} must fail-open: {e:?}", m.name));
    }
}

/// The pre-re-point orchestration on SQLite: plain set (gated `apply`) → each
/// rebuild (gate + `rebuild_one`), no renames. Built from the public primitives the
/// old `apply_declarative_locked` body used. Shares NO code with the live path.
async fn oracle_apply(
    engine: &MigrationEngine,
    plan: &DeclarativeDeployPlan,
    approval: Approval,
    be: &SqliteBackend,
    cfg: &ExecutorConfig,
) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
    if !plan.plain.denied.is_empty() {
        return Err(DeclarativeApplyError::Plain(EngineError::Denied(plan.plain.denied.clone())));
    }
    if plan.plain.requires_approval && approval != Approval::Approved {
        return Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired));
    }
    let mut applied = engine
        .apply(&plan.plain, approval, be, cfg, "deployer")
        .await
        .map_err(DeclarativeApplyError::Plain)?;
    if !plan.rebuilds.is_empty() && approval != Approval::Approved {
        return Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired));
    }
    for rebuild in &plan.rebuilds {
        // The trait method (returns ApplyError), matching the engine's seam — not
        // the inherent `SqliteBackend::rebuild_one` (which returns RebuildError).
        MigrationBackend::rebuild_one(be, &rebuild.spec, &rebuild.migration, &ApprovalScope::All, "deployer")
            .await
            .map_err(|e| DeclarativeApplyError::Plain(EngineError::Apply(e)))?;
        applied.applied.push(rebuild.migration.version.as_str().to_string());
    }
    assert!(plan.renames.is_empty(), "SQLite plans never carry PG renames");
    Ok(DeclarativeDeployOutcome { applied, pending_contract: Vec::new(), opened_obligations: Vec::new() })
}

fn people_v1() -> Vec<CollectionDescriptor> {
    vec![CollectionDescriptor {
        name: "people".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "nickname".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    }]
}

/// Build the second-deploy rename plan (nickname → handle) against a live snapshot.
fn rename_plan(engine: &MigrationEngine, live: &SchemaSnapshot, ownership: &HashMap<String, String>) -> DeclarativeDeployPlan {
    let mut v2 = people_v1();
    v2[0].fields[0].name = "handle".into();
    let desired2 = desired_snapshot(PROJECT, &v2).expect("v2 desired");
    let hint = RenameHint { table: "people".into(), from: "nickname".into(), to: "handle".into() };
    engine
        .plan_declarative(&desired2, live, ownership, &sqlite_author(), std::slice::from_ref(&hint), &guard_cfg())
        .expect("plan rename")
}

// ===========================================================================
// (b) SQLite RENAME-via-REBUILD — oracle vs live, frozen fixture.
// ===========================================================================

#[compio::test]
async fn golden_b_sqlite_rename_rebuild() {
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    async fn run_one(
        engine: &MigrationEngine,
        cfg: &ExecutorConfig,
        leg: &str,
    ) -> String {
        let p = paths(&format!("golden_b_{leg}"));
        let be = backend(&p);
        apply_first_deploy(&be, &people_v1()).await;
        // Seed rows so the rebuild carries data.
        be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
        be.actor()
            .exec("INSERT INTO main.people (id, nickname) VALUES ('p1','ada'),('p2','grace')")
            .await
            .expect("seed");
        let (live, ownership) = live_from(&people_v1());
        let plan = rename_plan(engine, &live, &ownership);
        assert_eq!(plan.rebuilds.len(), 1, "{leg}: rename → exactly one rebuild");
        assert!(plan.renames.is_empty(), "{leg}: no PG renames on SQLite");
        if leg == "oracle" {
            oracle_apply(engine, &plan, Approval::Approved, &be, cfg).await.expect("oracle rename");
        } else {
            engine
                .apply_declarative(&plan, Approval::Approved, &be, cfg, "deployer")
                .await
                .expect("live rename");
        }
        // Data followed the rename.
        let vals = be
            .actor()
            .query("SELECT handle FROM main.people ORDER BY id")
            .await
            .expect("read handle");
        assert_eq!(
            vals.iter().filter_map(|r| r[0].clone()).collect::<Vec<_>>(),
            vec!["ada", "grace"],
            "{leg}: the renamed column carries the data"
        );
        capture(&be).await
    }

    let cap_o = run_one(&engine, &cfg, "oracle").await;
    let cap_l = run_one(&engine, &cfg, "live").await;
    assert_eq!(
        cap_o, cap_l,
        "sqlite_b_rename_rebuild: the live re-pointed apply_declarative diverged from the oracle"
    );
    assert_frozen("sqlite_b_rename_rebuild", &cap_o);
}

// ===========================================================================
// (g) EMPTY-RENAMES FAIL-CLOSED — a PG expand-contract rename routed to a SQLite
//     backend (no online capability) must FAIL CLOSED at apply_plan, never drop it.
// ===========================================================================

#[compio::test]
async fn golden_g_sqlite_pg_rename_fails_closed() {
    use zeroship_migrate::plan::{PlanStep, RenameStep};
    use zeroship_migrate::ExpandContractAuthor;
    use zeroship_migrate::OnlineIntent;

    let engine = MigrationEngine::new();
    let cfg = exec_cfg();
    let p = paths("golden_g");
    let be = backend(&p);
    apply_first_deploy(&be, &people_v1()).await;

    // Author a PG expand-contract rename and feed it as a PlanStep to a SQLite
    // backend (online() == None). The differ never produces this on SQLite; we
    // construct it directly to prove the apply_plan dispatch fails closed rather
    // than silently dropping the rename (the §3.3 / H1 invariant).
    let rename = ExpandContractAuthor::new(PROJECT, APP)
        .author(&OnlineIntent::RenameColumn {
            table: "people".into(),
            from: "nickname".into(),
            to: "handle".into(),
            ty: "text".into(),
        })
        .expect("author rename");
    let steps = vec![PlanStep::OnlineRename(RenameStep::PgExpandContract(rename))];
    let res = engine
        .apply_plan(
            &steps,
            Approval::Approved,
            &be,
            &cfg,
            "deployer",
            zeroship_migrate::executor::LockMode::Acquire,
        )
        .await;
    assert!(
        matches!(
            res,
            Err(DeclarativeApplyError::Plain(EngineError::Apply(
                zeroship_migrate::executor::ApplyError::Backend(_)
            )))
        ),
        "a PG expand-contract rename on a SQLite backend must FAIL CLOSED (routing bug), got {res:?}"
    );
    // Nothing was journaled for the rename.
    let entries = be.applied_sqlite().await.expect("read journal");
    let people_only_create = entries.iter().all(|e| e.phase == Phase::Completed);
    assert!(people_only_create, "the fail-closed left the journal intact");
}
