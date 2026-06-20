//! PHASE 6a — the `MigrationEngine` drives SQLite end-to-end through a
//! `SqliteBackend`, against REAL temp-file SQLite (the faithful path: the actual
//! `DeclarativeAuthor` builds the plan, and the engine's generic `apply_declarative`
//! orchestrates the plain set + the 12-step rebuild under confinement + the `_mig`
//! journal). This proves the P6a lift: the engine is now generic over
//! `MigrationBackend`, the `plan_declarative` fail-close on SQLite rebuilds is gone,
//! and a rebuild is driven through `SqliteBackend::rebuild_one` under the
//! destructive/approval gate — NOT the direct executor-internal seam.
//!
//! Coverage (the P6a gate):
//! - the engine applies a declarative deploy with a rebuild (a column TYPE change)
//!   END-TO-END through `apply_declarative` — table rebuilt, data preserved,
//!   journaled `completed`;
//! - a clean RE-RUN is a no-op (idempotent) on the engine path (empty desired-vs-live
//!   diff AND the versioned-journal-completed rebuild path);
//! - a SQLite declarative deploy with a RENAME routes to a rebuild (not run_expand);
//!   `plan.renames` is empty for SQLite (H1);
//! - the destructive/approval gate is intact: a rebuild without `Approval::Approved`
//!   is refused (not auto-approved — the dev-relaxed posture is P6b).

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::journal::Phase;
use zeroship_migrate::{
    desired_snapshot, Approval, CollectionDescriptor, DeclarativeAuthor, DeclarativeApplyError,
    EngineError, ExecutorConfig, FieldDescriptor, GuardConfig, IndexDescriptor, MigrationEngine,
    RenameHint, SchemaSnapshot, SqliteBackend,
};
use zeroship_migrate::backend_sqlite::Mode;
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
    // SQLite ignores the schema strings (lock is a single-actor no-op, the journal
    // lives in the `_mig` attached file); the engine still needs a config to thread.
    ExecutorConfig::new(PROJECT, PROJECT)
}

fn guard_cfg() -> GuardConfig {
    GuardConfig::confined_sqlite(PROJECT)
}

/// Live snapshot + ownership reconstructed from a descriptor set (the `desired_snapshot`
/// machinery the engine uses), used as the "current live" for an incremental deploy.
fn live_from(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_snapshot(PROJECT, descs).expect("desired_snapshot");
    let ownership: HashMap<String, String> = d
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    (d.snapshot, ownership)
}

/// `PRAGMA main.table_info(<table>)` → the declared SQLite type of `column`.
async fn column_type(be: &SqliteBackend, table: &str, column: &str) -> String {
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let info = be
        .actor()
        .query(&format!("PRAGMA main.table_info({table})"))
        .await
        .expect("table_info");
    info.iter()
        .find(|r| r[1].as_deref() == Some(column))
        .and_then(|r| r[2].clone())
        .unwrap_or_default()
}

/// True iff `version` is journaled `completed` in the `_mig` net-state.
async fn is_completed(be: &SqliteBackend, version: &str) -> bool {
    be.applied_sqlite()
        .await
        .expect("read journal")
        .iter()
        .any(|e| e.version == version && e.phase == Phase::Completed)
}

// ---------------------------------------------------------------------------
// (1) END-TO-END: the ENGINE applies a declarative deploy that includes a REBUILD
//     (a column type change) through `apply_declarative` — the plain first deploy +
//     the rebuild second deploy, both driven by the engine over a SqliteBackend.
//     Data preserved, table rebuilt to the new shape, journaled `completed`.
//     This proves the fail-close is gone and the engine drives SQLite rebuilds.
// ---------------------------------------------------------------------------
#[compio::test]
async fn engine_applies_sqlite_rebuild_end_to_end() {
    // v1: accounts(amount: number → REAL affinity).
    let v1 = vec![CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "amount".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    }];
    // v2: the SAME column re-typed to string → TEXT affinity. A genuine type change
    // → the diff yields exactly one rebuild.
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("engine_rebuild_e2e");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // --- Deploy 1: create the table THROUGH THE ENGINE (plain additive set). ---
    let desired1 = desired_snapshot(PROJECT, &v1).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
        )
        .expect("plan create");
    assert!(plan1.rebuilds.is_empty(), "first deploy has no rebuild");
    engine
        .apply_declarative(&plan1, Approval::None, &be, &cfg, "deployer")
        .await
        .expect("engine applies the create-table deploy");

    // Seed rows into the live table (engine mode lets the test write `main`).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.accounts (id, amount) VALUES ('a1', 1), ('a2', 2), ('a3', 3)")
        .await
        .expect("seed rows");

    // The column is REAL-affinity before the rebuild.
    assert_eq!(
        column_type(&be, "accounts", "amount").await.to_ascii_uppercase(),
        "REAL",
        "amount is REAL (number) before the rebuild"
    );

    // --- Deploy 2: the type change. The engine PLANS a rebuild and DRIVES it. ---
    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2).expect("v2 desired");
    let plan2 = engine
        .plan_declarative(&desired2, &live, &ownership, &sqlite_author(), &[], &guard_cfg())
        .expect("plan_declarative carries the rebuild (no fail-close)");
    assert_eq!(plan2.rebuilds.len(), 1, "the type change yields one rebuild");
    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();

    // Drive it through the GENERIC public engine path. A rebuild is destructive ⇒
    // the gate demands approval; supply it (the caller's decision — NOT auto-approved).
    let outcome = engine
        .apply_declarative(&plan2, Approval::Approved, &be, &cfg, "deployer")
        .await
        .expect("engine drives the SQLite rebuild end-to-end");
    assert!(
        outcome.applied.applied.contains(&rebuild_version),
        "the rebuild version is reported applied by the engine"
    );

    // Data is preserved (all three rows survived the copy).
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.accounts")
        .await
        .expect("count rows");
    assert_eq!(count[0][0].as_deref(), Some("3"), "all rows preserved");

    // The table shape is the NEW schema: `amount` now has TEXT affinity.
    assert_eq!(
        column_type(&be, "accounts", "amount").await.to_ascii_uppercase(),
        "TEXT",
        "the rebuilt column has the new TEXT type"
    );

    // The rebuild is journaled `completed` (a real `_mig` versioned journal row).
    assert!(
        is_completed(&be, &rebuild_version).await,
        "the engine-driven rebuild is journaled completed"
    );
}

// ---------------------------------------------------------------------------
// (2) IDEMPOTENT RE-RUN: re-planning + re-applying the SAME desired-vs-live state is
//     a NO-OP on the engine path — BOTH because (a) once the rebuild is live, the
//     desired-vs-live diff is empty (no rebuild planned), AND (b) even if the SAME
//     rebuild plan instance is re-applied, the versioned-journal `completed` gate
//     skips it. Both arms are asserted.
// ---------------------------------------------------------------------------
#[compio::test]
async fn engine_sqlite_rebuild_rerun_is_a_noop() {
    let v1 = vec![CollectionDescriptor {
        name: "ledger".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "balance".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("engine_rebuild_rerun");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // Deploy 1 (create) + seed a row.
    let desired1 = desired_snapshot(PROJECT, &v1).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
        )
        .expect("plan create");
    engine
        .apply_declarative(&plan1, Approval::None, &be, &cfg, "deployer")
        .await
        .expect("apply create");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.ledger (id, balance) VALUES ('r1', 42)")
        .await
        .expect("seed");

    // Deploy 2: the rebuild. Apply it once.
    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2).expect("v2 desired");
    let plan2 = engine
        .plan_declarative(&desired2, &live, &ownership, &sqlite_author(), &[], &guard_cfg())
        .expect("plan rebuild");
    assert_eq!(plan2.rebuilds.len(), 1);
    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();
    let first = engine
        .apply_declarative(&plan2, Approval::Approved, &be, &cfg, "deployer")
        .await
        .expect("first rebuild apply");
    assert!(first.applied.applied.contains(&rebuild_version), "applied once");

    // (b) Re-apply the SAME plan instance: the versioned-journal `completed` gate
    //     skips it — applied stays empty, the rebuild version is reported skipped.
    let second = engine
        .apply_declarative(&plan2, Approval::Approved, &be, &cfg, "deployer")
        .await
        .expect("re-applying the same rebuild plan is a no-op");
    assert!(
        !second.applied.applied.contains(&rebuild_version),
        "the completed rebuild is NOT re-applied (idempotent journal gate)"
    );
    assert!(
        second.applied.skipped.contains(&rebuild_version),
        "the completed rebuild is reported skipped on re-run"
    );

    // (a) Re-plan from the NOW-LIVE state: desired == live ⇒ the diff is empty, so
    //     the plan carries NO rebuild at all.
    let (live2, ownership2) = live_from(&v2);
    let plan_rerun = engine
        .plan_declarative(&desired2, &live2, &ownership2, &sqlite_author(), &[], &guard_cfg())
        .expect("re-plan from live");
    assert!(
        plan_rerun.rebuilds.is_empty() && plan_rerun.plain.items.is_empty(),
        "a re-plan against the reconciled live state yields an empty diff"
    );

    // The row + the new shape survived the idempotent re-runs.
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.ledger")
        .await
        .expect("count");
    assert_eq!(count[0][0].as_deref(), Some("1"), "row preserved across re-runs");
    assert_eq!(
        column_type(&be, "ledger", "balance").await.to_ascii_uppercase(),
        "TEXT",
        "the new shape is stable across re-runs"
    );
}

// ---------------------------------------------------------------------------
// (3) RENAME ROUTES TO A REBUILD (H1): a SQLite declarative deploy with a hinted
//     column RENAME is reconciled by a 12-step REBUILD (copying `to ← from`), NOT the
//     PG-shaped expand-contract `run_expand`. `plan.renames` is EMPTY for SQLite, so
//     `run_expand` is never reached. The engine applies the rebuild and the data
//     follows the rename.
// ---------------------------------------------------------------------------
#[compio::test]
async fn engine_sqlite_rename_routes_to_rebuild_not_run_expand() {
    // v1: contacts(email). v2: the same column renamed to email_address (hinted).
    let v1 = vec![CollectionDescriptor {
        name: "contacts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "email".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].name = "email_address".into();

    let p = paths("engine_rename_rebuild");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // Deploy 1: create + seed a row whose `email` value must follow the rename.
    let desired1 = desired_snapshot(PROJECT, &v1).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
        )
        .expect("plan create");
    engine
        .apply_declarative(&plan1, Approval::None, &be, &cfg, "deployer")
        .await
        .expect("apply create");
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    be.actor()
        .exec("INSERT INTO main.contacts (id, email) VALUES ('c1', 'a@b.test')")
        .await
        .expect("seed");

    // Deploy 2: the RENAME, expressed as a hint (the creator's signed intent).
    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2).expect("v2 desired");
    let hints = vec![RenameHint {
        table: "contacts".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];
    let plan2 = engine
        .plan_declarative(&desired2, &live, &ownership, &sqlite_author(), &hints, &guard_cfg())
        .expect("plan rename");

    // H1 — the rename is a REBUILD, NOT an online expand-contract: renames is EMPTY,
    // rebuilds carries the one rebuild. `run_expand` can never be reached.
    assert!(
        plan2.renames.is_empty(),
        "a SQLite rename must NOT become an expand-contract rename (run_expand); it is a rebuild"
    );
    assert_eq!(
        plan2.rebuilds.len(),
        1,
        "the SQLite rename is reconciled by exactly one rebuild"
    );

    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();
    engine
        .apply_declarative(&plan2, Approval::Approved, &be, &cfg, "deployer")
        .await
        .expect("engine drives the rename-rebuild");

    // The column is renamed and the data FOLLOWED the rename (to ← from copy).
    assert_eq!(
        column_type(&be, "contacts", "email_address").await.to_ascii_uppercase(),
        "TEXT",
        "the renamed-to column exists with its type"
    );
    be.actor().set_mode(Mode::EngineJournal).await.expect("mode");
    let val = be
        .actor()
        .query("SELECT email_address FROM main.contacts WHERE id = 'c1'")
        .await
        .expect("read renamed column");
    assert_eq!(
        val.first().and_then(|r| r.first()).and_then(|c| c.clone()).as_deref(),
        Some("a@b.test"),
        "the value followed the rename into email_address"
    );
    // The old column is gone.
    assert!(
        column_type(&be, "contacts", "email").await.is_empty(),
        "the old `email` column no longer exists after the rename-rebuild"
    );
    assert!(
        is_completed(&be, &rebuild_version).await,
        "the rename-rebuild is journaled completed"
    );
}

// ---------------------------------------------------------------------------
// (4) APPROVAL GATE INTACT: a SQLite rebuild (destructive) is REFUSED without
//     `Approval::Approved` — the engine does NOT auto-approve (the dev-relaxed
//     posture is P6b's concern). Nothing is applied; the live shape is unchanged.
// ---------------------------------------------------------------------------
#[compio::test]
async fn engine_sqlite_rebuild_refused_without_approval() {
    let v1 = vec![CollectionDescriptor {
        name: "items".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "qty".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![IndexDescriptor {
            name: "items_qty_idx".into(),
            columns: vec!["qty".into()],
            unique: false,
        }],
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("engine_rebuild_gate");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    let desired1 = desired_snapshot(PROJECT, &v1).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
        )
        .expect("plan create");
    engine
        .apply_declarative(&plan1, Approval::None, &be, &cfg, "deployer")
        .await
        .expect("apply create");

    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2).expect("v2 desired");
    let plan2 = engine
        .plan_declarative(&desired2, &live, &ownership, &sqlite_author(), &[], &guard_cfg())
        .expect("plan rebuild");
    assert_eq!(plan2.rebuilds.len(), 1);
    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();

    // Apply WITHOUT approval — the engine must refuse (ApprovalRequired), not auto-approve.
    let err = engine
        .apply_declarative(&plan2, Approval::None, &be, &cfg, "deployer")
        .await
        .expect_err("a destructive rebuild without approval must be refused");
    assert!(
        matches!(
            err,
            DeclarativeApplyError::Plain(EngineError::ApprovalRequired)
        ),
        "expected ApprovalRequired, got {err:?}"
    );

    // Nothing applied: the column is still REAL-affinity (number), no rebuild ran.
    assert_eq!(
        column_type(&be, "items", "qty").await.to_ascii_uppercase(),
        "REAL",
        "the live shape is unchanged — the rebuild did not run"
    );
    assert!(
        !is_completed(&be, &rebuild_version).await,
        "the rebuild is NOT journaled (it was refused before any DDL)"
    );
}
