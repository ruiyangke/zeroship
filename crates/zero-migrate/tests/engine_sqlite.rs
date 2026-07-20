//! The `MigrationEngine` drives `SQLite` end-to-end through a
//! `SqliteBackend`, against REAL temp-file `SQLite` (the faithful path: the actual
//! `DeclarativeAuthor` builds the plan, and the engine's generic `apply_declarative`
//! orchestrates the plain set + the 12-step rebuild under confinement + the `_mig`
//! journal). This proves the engine is now generic over
//! `MigrationBackend`, the `plan_declarative` fail-close on `SQLite` rebuilds is gone,
//! and a rebuild is driven through `SqliteBackend::rebuild_one` under the
//! destructive/approval gate — NOT the direct executor-internal seam.
//!
//! Coverage:
//! - the engine applies a declarative deploy with a rebuild (a column TYPE change)
//!   END-TO-END through `apply_declarative` — table rebuilt, data preserved,
//!   journaled `completed`;
//! - a clean RE-RUN is a no-op (idempotent) on the engine path (empty desired-vs-live
//!   diff AND the versioned-journal-completed rebuild path);
//! - a `SQLite` declarative deploy with a RENAME routes to a rebuild (not `run_expand`);
//!   `plan.renames` is empty for `SQLite`;
//! - the destructive/approval gate is intact: a rebuild without `Approval::Approved`
//!   is refused (not auto-approved — the dev-relaxed posture is handled separately).

mod support;

use std::collections::HashMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::journal::{JournaledKind, Phase};
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{
    Approval, CollectionDescriptor, DeclarativeApplyError, DeclarativeAuthor, DryRunError,
    EffectivePolicy, EngineError, ExecutorConfig, FieldDescriptor, GuardConfig, IndexDescriptor,
    MigrationBackend, MigrationEngine, RenameHint, SchemaSnapshot, ShadowConfig, SqliteBackend,
};

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
    ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT))
}

fn guard_cfg() -> GuardConfig {
    GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite)
}

fn effective_policy() -> EffectivePolicy {
    support::confined_charter()
}

fn desired_snapshot(
    project_schema: &str,
    descriptors: &[CollectionDescriptor],
    effective: &EffectivePolicy,
) -> Result<zero_migrate::DesiredSchema, zero_migrate::DeclarativeError> {
    zero_migrate::desired_snapshot_for_dialect(
        project_schema,
        descriptors,
        SqlDialect::Sqlite,
        effective,
    )
}

/// Live snapshot + ownership reconstructed from a descriptor set (the `desired_snapshot`
/// machinery the engine uses), used as the "current live" for an incremental deploy.
fn live_from(descs: &[CollectionDescriptor]) -> (SchemaSnapshot, HashMap<String, String>) {
    let d = desired_snapshot(PROJECT, descs, &effective_policy()).expect("desired_snapshot");
    let ownership: HashMap<String, String> = d
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    (d.snapshot, ownership)
}

/// `PRAGMA main.table_info(<table>)` → the declared `SQLite` type of `column`.
async fn column_type(be: &SqliteBackend, table: &str, column: &str) -> String {
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
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
        runtime_options: Default::default(),
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
    let desired1 = desired_snapshot(PROJECT, &v1, &effective_policy()).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan create");
    assert!(plan1.rebuilds.is_empty(), "first deploy has no rebuild");
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("engine applies the create-table deploy");

    // Seed rows into the live table (engine mode lets the test write `main`).
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.accounts \
             (id, created_at, updated_at, version, amount) VALUES \
             ('a1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 1), \
             ('a2', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 2), \
             ('a3', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 3)",
        )
        .await
        .expect("seed rows");

    // The column is REAL-affinity before the rebuild.
    assert_eq!(
        column_type(&be, "accounts", "amount")
            .await
            .to_ascii_uppercase(),
        "REAL",
        "amount is REAL (number) before the rebuild"
    );

    // --- Deploy 2: the type change. The engine PLANS a rebuild and DRIVES it. ---
    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2, &effective_policy()).expect("v2 desired");
    let plan2 = engine
        .plan_declarative(
            &desired2,
            &live,
            &ownership,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan_declarative carries the rebuild (no fail-close)");
    assert_eq!(
        plan2.rebuilds.len(),
        1,
        "the type change yields one rebuild"
    );
    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();

    // Drive it through the GENERIC public engine path. A rebuild is destructive ⇒
    // the gate demands approval; supply it (the caller's decision — NOT auto-approved).
    let outcome = engine
        .apply_declarative(
            &plan2,
            &effective_policy(),
            Approval::Approved,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("engine drives the SQLite rebuild end-to-end");
    assert!(
        outcome.applied.applied.contains(&rebuild_version),
        "the rebuild version is reported applied by the engine"
    );

    // Data is preserved (all three rows survived the copy).
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.accounts")
        .await
        .expect("count rows");
    assert_eq!(count[0][0].as_deref(), Some("3"), "all rows preserved");

    // The table shape is the NEW schema: `amount` now has TEXT affinity.
    assert_eq!(
        column_type(&be, "accounts", "amount")
            .await
            .to_ascii_uppercase(),
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
        runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("engine_rebuild_rerun");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // Deploy 1 (create) + seed a row.
    let desired1 = desired_snapshot(PROJECT, &v1, &effective_policy()).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan create");
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply create");
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.ledger \
             (id, created_at, updated_at, version, balance) VALUES \
             ('r1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 42)",
        )
        .await
        .expect("seed");

    // Deploy 2: the rebuild. Apply it once.
    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2, &effective_policy()).expect("v2 desired");
    let plan2 = engine
        .plan_declarative(
            &desired2,
            &live,
            &ownership,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan rebuild");
    assert_eq!(plan2.rebuilds.len(), 1);
    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();
    let first = engine
        .apply_declarative(
            &plan2,
            &effective_policy(),
            Approval::Approved,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("first rebuild apply");
    assert!(
        first.applied.applied.contains(&rebuild_version),
        "applied once"
    );

    // (b) Re-apply the SAME plan instance: the versioned-journal `completed` gate
    //     skips it — applied stays empty, the rebuild version is reported skipped.
    let second = engine
        .apply_declarative(
            &plan2,
            &effective_policy(),
            Approval::Approved,
            &be,
            &cfg,
            "deployer",
        )
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
        .plan_declarative(
            &desired2,
            &live2,
            &ownership2,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("re-plan from live");
    assert!(
        plan_rerun.rebuilds.is_empty() && plan_rerun.plain.items.is_empty(),
        "a re-plan against the reconciled live state yields an empty diff"
    );

    // The row + the new shape survived the idempotent re-runs.
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.ledger")
        .await
        .expect("count");
    assert_eq!(
        count[0][0].as_deref(),
        Some("1"),
        "row preserved across re-runs"
    );
    assert_eq!(
        column_type(&be, "ledger", "balance")
            .await
            .to_ascii_uppercase(),
        "TEXT",
        "the new shape is stable across re-runs"
    );
}

// ---------------------------------------------------------------------------
// (3) RENAME ROUTES TO A REBUILD: a SQLite declarative deploy with a hinted
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
        runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].name = "email_address".into();

    let p = paths("engine_rename_rebuild");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // Deploy 1: create + seed a row whose `email` value must follow the rename.
    let desired1 = desired_snapshot(PROJECT, &v1, &effective_policy()).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan create");
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply create");
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.contacts \
             (id, created_at, updated_at, version, email) VALUES \
             ('c1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 'a@b.test')",
        )
        .await
        .expect("seed");

    // Deploy 2: the RENAME, expressed as a hint (the creator's signed intent).
    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2, &effective_policy()).expect("v2 desired");
    let hints = vec![RenameHint {
        table: "contacts".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];
    let plan2 = engine
        .plan_declarative(
            &desired2,
            &live,
            &ownership,
            &sqlite_author(),
            &hints,
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan rename");

    // The rename is a REBUILD, NOT an online expand-contract: renames is EMPTY,
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

    // The SQLite backend has NO online schema-change capability: `online()`
    // is `None`. This is the honest capability the old `expand_conn() == None`
    // sentinel became — paired with the renames-empty invariant above, it proves the
    // online path is structurally never reached on SQLite (no `compio_postgres::Client`
    // anywhere in the SQLite leg).
    assert!(
        be.online().is_none(),
        "SQLite exposes no OnlineSchemaChange capability — online() must be None"
    );

    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();
    engine
        .apply_declarative(
            &plan2,
            &effective_policy(),
            Approval::Approved,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("engine drives the rename-rebuild");

    // The column is renamed and the data FOLLOWED the rename (to ← from copy).
    assert_eq!(
        column_type(&be, "contacts", "email_address")
            .await
            .to_ascii_uppercase(),
        "TEXT",
        "the renamed-to column exists with its type"
    );
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let val = be
        .actor()
        .query("SELECT email_address FROM main.contacts WHERE id = 'c1'")
        .await
        .expect("read renamed column");
    assert_eq!(
        val.first()
            .and_then(|r| r.first())
            .and_then(std::clone::Clone::clone)
            .as_deref(),
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
//     posture is handled separately). Nothing is applied; the live shape is unchanged.
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
        runtime_options: Default::default(),
    }];
    let mut v2 = v1.clone();
    v2[0].fields[0].ty = "string".into();

    let p = paths("engine_rebuild_gate");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    let desired1 = desired_snapshot(PROJECT, &v1, &effective_policy()).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan create");
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply create");

    let (live, ownership) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2, &effective_policy()).expect("v2 desired");
    let plan2 = engine
        .plan_declarative(
            &desired2,
            &live,
            &ownership,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan rebuild");
    assert_eq!(plan2.rebuilds.len(), 1);
    let rebuild_version = plan2.rebuilds[0].migration.version.as_str().to_string();

    // Apply WITHOUT approval — the engine must refuse (ApprovalRequired), not auto-approve.
    let err = engine
        .apply_declarative(
            &plan2,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
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

// ---------------------------------------------------------------------------
// (ROLL-FORWARD over destructive history) — the SQLite leg has NO rollback for a
// rebuild (no shadow clone; `online()`/`shadow()` are None). The engine's stance
// is ROLL-FORWARD: a destructive migration in the applied history is compensated
// by a LATER forward migration, never an unwind. This mirrors the PG
// lifecycle/rollback roll-forward semantics on the SQLite backend.
//
// Sequence (all forward, in order, on ONE backend):
//   v1  create `accounts(amount: number, legacy: string)` + seed rows;
//   v2  DESTRUCTIVE: drop `legacy` (a rebuild — destructive + approval-gated),
//       APPROVED at deploy → the column is gone, rows preserved;
//   v3  additive forward: add `note` — built on the v2 (post-destructive) shape.
// End-state: `legacy` gone, `note` present, original `amount` data intact, and
// EVERY step journaled `completed` (nothing rolled back).
// ---------------------------------------------------------------------------
#[compio::test]
async fn roll_forward_over_destructive_history_on_sqlite() {
    // v1: accounts(amount: number, legacy: string).
    let v1 = vec![CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "amount".into(),
                ty: "number".into(),
                required: true,
                ..Default::default()
            },
            // `legacy` carries a `min` → an inline CHECK constraint, so dropping it
            // CANNOT use native `ALTER … DROP COLUMN` (SQLite errors on a
            // CHECK-constrained column) and MUST route through the 12-step rebuild —
            // i.e. a genuinely destructive, approval-gated step in the history.
            FieldDescriptor {
                name: "legacy".into(),
                ty: "number".into(),
                required: true,
                min: Some(0.0),
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }];
    // v2: drop `legacy` (destructive — a rebuild on SQLite, CHECK-constrained).
    let v2 = vec![CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "amount".into(),
            ty: "number".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }];
    // v3: add `note` on top of the post-destructive shape (additive forward).
    let v3 = vec![CollectionDescriptor {
        name: "accounts".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "amount".into(),
                ty: "number".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "note".into(),
                ty: "string".into(),
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }];

    let p = paths("roll_forward_destructive");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // --- v1: create + seed. ---
    let desired1 = desired_snapshot(PROJECT, &v1, &effective_policy()).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan v1");
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply v1");
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.accounts \
             (id, created_at, updated_at, version, amount, legacy) VALUES \
             ('a1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 10, 1), \
             ('a2', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 20, 2)",
        )
        .await
        .expect("seed rows");

    // --- v2: the DESTRUCTIVE drop of `legacy` (a rebuild), APPROVED. ---
    let (live1, own1) = live_from(&v1);
    let desired2 = desired_snapshot(PROJECT, &v2, &effective_policy()).expect("v2 desired");
    let plan2 = engine
        .plan_declarative(
            &desired2,
            &live1,
            &own1,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan v2");
    assert_eq!(plan2.rebuilds.len(), 1, "the destructive drop is a rebuild");
    let v2_version = plan2.rebuilds[0].migration.version.as_str().to_string();
    assert!(
        plan2.rebuilds[0].migration.flags.destructive
            && plan2.rebuilds[0].migration.flags.requires_approval,
        "the legacy-column drop is destructive + approval-gated"
    );
    // Roll-FORWARD: we approve and APPLY the destructive step (not roll anything back).
    engine
        .apply_declarative(
            &plan2,
            &effective_policy(),
            Approval::Approved,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply the approved destructive rebuild (roll-forward)");

    // `legacy` is gone; the `amount` data is intact (rows carried through the rebuild).
    assert!(
        column_type(&be, "accounts", "legacy").await.is_empty(),
        "the destructive drop removed `legacy`"
    );
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let amounts = be
        .actor()
        .query("SELECT amount FROM main.accounts ORDER BY id")
        .await
        .expect("read amount");
    assert_eq!(
        amounts
            .iter()
            .filter_map(|r| r[0].clone())
            .collect::<Vec<_>>(),
        vec!["10", "20"],
        "the surviving `amount` data carried through the destructive rebuild"
    );

    // --- v3: an additive forward migration on the POST-destructive shape. ---
    let (live2, own2) = live_from(&v2);
    let desired3 = desired_snapshot(PROJECT, &v3, &effective_policy()).expect("v3 desired");
    let plan3 = engine
        .plan_declarative(
            &desired3,
            &live2,
            &own2,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan v3");
    assert!(
        plan3.rebuilds.is_empty(),
        "v3 is a plain additive forward step"
    );
    let v3_version = plan3
        .plain
        .items
        .iter()
        .map(|i| i.migration.version.as_str().to_string())
        .collect::<Vec<_>>();
    engine
        .apply_declarative(
            &plan3,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply v3 (additive forward, no rollback)");

    // End-state: `note` present, `legacy` still gone, data intact.
    assert!(
        !column_type(&be, "accounts", "note").await.is_empty(),
        "the additive forward column landed on the post-destructive shape"
    );
    assert!(
        column_type(&be, "accounts", "legacy").await.is_empty(),
        "the dropped column stays gone (no rollback resurrected it)"
    );
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.accounts")
        .await
        .expect("count");
    assert_eq!(
        count[0][0].as_deref(),
        Some("2"),
        "rows survived the whole roll-forward history"
    );

    // EVERY step is journaled `completed` — nothing was rolled back.
    assert!(
        is_completed(&be, &v2_version).await,
        "the destructive rebuild is completed"
    );
    for v in &v3_version {
        assert!(
            is_completed(&be, v).await,
            "the additive forward step {v} is completed"
        );
    }
}

// ---------------------------------------------------------------------------
// (WARM MULTI-COLLECTION BOOT) — a deploy with 2+ collections, then a "fresh
// isolate" RE-DEPLOY of the SAME declared set against the live file. The warm
// re-plan against the REAL introspected live schema yields an EMPTY diff — no
// spurious DROP of either table — and BOTH tables stay usable (a write into each
// succeeds). This is the declared-set path on the SQLite schema authority (the
// migrate engine; `register_model`/`run_pipeline` is PG-only by construction —
// its `RegisterBackend` bound requires `PgSqlExecutor` +
// `LockManager<Client = compio_postgres::Client>`, which SQLite does not satisfy,
// so the warm-boot authority on SQLite is the engine, not the runtime pipeline).
// ---------------------------------------------------------------------------
#[compio::test]
async fn warm_multi_collection_reboot_no_spurious_drop_both_usable() {
    // Two collections in one declared set.
    let set = || {
        vec![
            CollectionDescriptor {
                name: "users".into(),
                owner_app: APP.into(),
                fields: vec![FieldDescriptor {
                    name: "handle".into(),
                    ty: "string".into(),
                    required: true,
                    ..Default::default()
                }],
                indexes: vec![],
                runtime_options: Default::default(),
            },
            CollectionDescriptor {
                name: "posts".into(),
                owner_app: APP.into(),
                fields: vec![FieldDescriptor {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
                    ..Default::default()
                }],
                indexes: vec![],
                runtime_options: Default::default(),
            },
        ]
    };

    let p = paths("warm_multi_boot");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // --- Cold deploy: create both collections. ---
    let desired1 = desired_snapshot(PROJECT, &set(), &effective_policy()).expect("desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan cold");
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply cold deploy");

    // Both tables exist + seed one row in each.
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    for t in ["users", "posts"] {
        let rows = be
            .actor()
            .query(&format!(
                "SELECT name FROM main.sqlite_master WHERE type='table' AND name='{t}'"
            ))
            .await
            .expect("introspect");
        assert_eq!(rows.len(), 1, "table {t} must exist after the cold deploy");
    }
    be.actor()
        .exec(
            "INSERT INTO main.users \
             (id, created_at, updated_at, version, handle) VALUES \
             ('u1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 'ada')",
        )
        .await
        .expect("seed users");
    be.actor()
        .exec(
            "INSERT INTO main.posts \
             (id, created_at, updated_at, version, title) VALUES \
             ('p1', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 'hello')",
        )
        .await
        .expect("seed posts");

    // --- WARM reboot: a fresh isolate re-introspects the live file and re-plans the
    //     SAME declared set. The diff against the REAL live schema must be EMPTY —
    //     no spurious drop of either table (the declared-set guard). ---
    let live = be.snapshot_schema_sqlite().await.expect("introspect live");
    let own: HashMap<String, String> = desired1
        .ownership
        .iter()
        .map(|(t, a)| (t.clone(), a.clone()))
        .collect();
    let desired2 = desired_snapshot(PROJECT, &set(), &effective_policy()).expect("re-desired");
    let plan2 = engine
        .plan_declarative(
            &desired2,
            &live,
            &own,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("warm re-plan");
    assert!(
        plan2.plain.items.is_empty() && plan2.rebuilds.is_empty(),
        "a warm reboot of the same declared set must yield an EMPTY diff (no spurious \
         drop/add/rebuild); got plain={:?} rebuilds={}",
        plan2
            .plain
            .items
            .iter()
            .map(|i| i.migration.name.clone())
            .collect::<Vec<_>>(),
        plan2.rebuilds.len()
    );
    // Applying the empty plan is a clean no-op.
    engine
        .apply_declarative(
            &plan2,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("applying the empty warm-reboot plan is a no-op");

    // BOTH tables are still present + usable after the warm reboot: the seeded rows
    // survive AND a fresh write into each succeeds (no table was dropped/recreated).
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    be.actor()
        .exec(
            "INSERT INTO main.users \
             (id, created_at, updated_at, version, handle) VALUES \
             ('u2', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 'grace')",
        )
        .await
        .expect("users still usable after warm reboot");
    be.actor()
        .exec(
            "INSERT INTO main.posts \
             (id, created_at, updated_at, version, title) VALUES \
             ('p2', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1, 'world')",
        )
        .await
        .expect("posts still usable after warm reboot");
    let users_n = be
        .actor()
        .query("SELECT COUNT(*) FROM main.users")
        .await
        .expect("count users");
    let posts_n = be
        .actor()
        .query("SELECT COUNT(*) FROM main.posts")
        .await
        .expect("count posts");
    assert_eq!(
        users_n[0][0].as_deref(),
        Some("2"),
        "users: seeded + new row both present"
    );
    assert_eq!(
        posts_n[0][0].as_deref(),
        Some("2"),
        "posts: seeded + new row both present"
    );
}

// ---------------------------------------------------------------------------
// (5) BASELINE: a SQLite baseline records the live schema as a `'baseline'`
//     journal entry WITHOUT running its `up`, adopting an existing journal-less
//     file. First-entry semantics: idempotent for the same version, refuses a
//     different baseline once one exists. This is the new SQLite `baseline`
//     (the PG `baseline()` is &Client-typed and previously had no SQLite peer).
// ---------------------------------------------------------------------------

/// A baseline migration whose `up` DOCUMENTS the live schema (recorded, not run).
fn baseline_migration(name: &str, up: &str) -> Migration {
    let flags = MigrationFlags::default();
    let owner_app = APP;
    let mut m = Migration {
        version: MigrationId::generate(),
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags,
        owner_app: owner_app.to_string(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        existence_guard: None,
    };
    m.recompute_checksum();
    m
}

#[compio::test]
async fn sqlite_baseline_adopts_a_journal_less_file_then_additive_deploy_works() {
    let p = paths("baseline_adopt");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // Simulate a file a prior `run_sqlite_pipeline` populated: a real table on
    // `main`, but an EMPTY `_mig` journal. Create the table directly (engine
    // mode lets the test write `main`); do NOT journal it.
    be.ensure_journal_sqlite().await.expect("ensure journal");
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    be.actor()
        .exec("CREATE TABLE main.widgets (id TEXT PRIMARY KEY, label TEXT)")
        .await
        .expect("seed legacy table");
    be.actor()
        .exec("INSERT INTO main.widgets (id, label) VALUES ('w1', 'legacy')")
        .await
        .expect("seed legacy row");
    assert!(
        be.applied_sqlite().await.expect("read journal").is_empty(),
        "the legacy file has tables but an EMPTY journal (the run_sqlite_pipeline shape)"
    );

    // Baseline: adopt the live schema. The `up` documents the table; it is NOT run.
    let base = baseline_migration(
        "baseline_adopt_widgets",
        "CREATE TABLE main.widgets (id TEXT PRIMARY KEY, label TEXT)",
    );
    let base_version = base.version.as_str().to_string();
    let outcome = be
        .baseline_one(&cfg, &base, "deployer")
        .await
        .expect("baseline adopts the live schema");
    assert!(!outcome.already_present, "first baseline is newly recorded");

    // The baseline is journaled `completed` with kind=baseline, and the live
    // table + its data are untouched (the `up` never ran a second CREATE).
    let entries = be.applied_sqlite().await.expect("read journal");
    let entry = entries
        .iter()
        .find(|e| e.version == base_version)
        .expect("baseline is in the journal");
    assert_eq!(entry.phase, Phase::Completed);
    assert_eq!(entry.kind, Some(JournaledKind::Baseline));
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let count = be
        .actor()
        .query("SELECT COUNT(*) FROM main.widgets")
        .await
        .expect("count");
    assert_eq!(
        count[0][0].as_deref(),
        Some("1"),
        "the legacy row survived adoption"
    );

    // Re-baselining the SAME version is an idempotent no-op (a retried boot).
    let again = be
        .baseline_one(&cfg, &base, "deployer")
        .await
        .expect("re-baseline same version is idempotent");
    assert!(again.already_present, "the same baseline is idempotent");

    // An additive deploy ON TOP of the baseline: add a `widgets.qty` column via
    // the engine. The diff against live yields one ADD COLUMN plain op; it
    // applies cleanly with NO drift/collision on the adopted table.
    let v2 = vec![CollectionDescriptor {
        name: "widgets".into(),
        owner_app: APP.into(),
        fields: vec![
            FieldDescriptor {
                name: "label".into(),
                ty: "string".into(),
                ..Default::default()
            },
            FieldDescriptor {
                name: "qty".into(),
                ty: "number".into(),
                ..Default::default()
            },
        ],
        indexes: vec![],
        runtime_options: Default::default(),
    }];
    let (live, ownership) = live_from(&[CollectionDescriptor {
        name: "widgets".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "label".into(),
            ty: "string".into(),
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }]);
    let desired2 = desired_snapshot(PROJECT, &v2, &effective_policy()).expect("v2 desired");
    let plan = engine
        .plan_declarative(
            &desired2,
            &live,
            &ownership,
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan additive on top of baseline");
    engine
        .apply_declarative(
            &plan,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("additive deploy applies cleanly on the baselined file");
    assert!(
        !column_type(&be, "widgets", "qty").await.is_empty(),
        "the additive column landed on the adopted table (no drift/collision)"
    );
    // And the legacy data still survives the additive deploy on the adopted file.
    be.actor()
        .set_mode(Mode::EngineJournal)
        .await
        .expect("mode");
    let count2 = be
        .actor()
        .query("SELECT COUNT(*) FROM main.widgets")
        .await
        .expect("count after additive");
    assert_eq!(
        count2[0][0].as_deref(),
        Some("1"),
        "legacy row survives the additive deploy"
    );
}

#[compio::test]
async fn sqlite_baseline_refuses_when_engine_already_manages_the_file() {
    let p = paths("baseline_refuse");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // Apply a real (non-baseline) deploy through the engine first, so the journal
    // records a net-applied `apply` migration — the file is now engine-managed.
    let v1 = vec![CollectionDescriptor {
        name: "gadgets".into(),
        owner_app: APP.into(),
        fields: vec![FieldDescriptor {
            name: "name".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }];
    let desired1 = desired_snapshot(PROJECT, &v1, &effective_policy()).expect("v1 desired");
    let plan1 = engine
        .plan_declarative(
            &desired1,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan create");
    engine
        .apply_declarative(
            &plan1,
            &effective_policy(),
            Approval::None,
            &be,
            &cfg,
            "deployer",
        )
        .await
        .expect("apply create");

    // A baseline on a file the engine already manages is refused (first-entry-only).
    let base = baseline_migration("late_baseline", "CREATE TABLE main.gadgets (id TEXT)");
    let err = be
        .baseline_one(&cfg, &base, "deployer")
        .await
        .expect_err("baseline must refuse an already-managed file");
    assert!(
        err.to_string().contains("first-entry") || err.to_string().contains("already"),
        "expected a first-entry refusal, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// SHADOW DRY-RUN CAPABILITY GAP: the SQLite backend has NO shadow dry-run
//      capability (`shadow()` is `None`), and the engine's `dry_run` /
//      `dry_run_declarative` surface the EXPLICIT `DryRunError::ShadowUnsupported`
//      — NOT a false-success `DryRunReport`. This is the honest outcome of the
//      deliberate capability gap (SQLite DDL is trusted + dev-recoverable, so no
//      pre-apply shadow clone): the caller must never believe a dry-run happened
//      when it did not.
// ---------------------------------------------------------------------------
#[compio::test]
async fn sqlite_backend_has_no_shadow_and_dry_run_is_explicitly_unsupported() {
    let p = paths("shadow_unsupported");
    let be = backend(&p);
    let engine = MigrationEngine::new();
    let cfg = exec_cfg();

    // The capability itself is absent — a deliberate `None`, not a stub.
    assert!(
        be.shadow().is_none(),
        "the SQLite backend exposes NO shadow dry-run capability (C3)"
    );

    let shadow_cfg = ShadowConfig {
        // Unused — `dry_run` short-circuits to ShadowUnsupported before any DSN is
        // touched (there is no PG admin connection on the SQLite leg).
        admin_dsn: String::new(),
        db_name_prefix: "zsmig_shadow_".to_string(),
    };

    // (a) `dry_run` over a plain migration set returns ShadowUnsupported — an
    //     explicit error, asserted, NOT a `DryRunReport { ok: true, .. }`.
    let mig = baseline_migration("any", "CREATE TABLE main.t (id TEXT PRIMARY KEY)");
    let err = engine
        .dry_run(&be, &[mig], &cfg, &shadow_cfg, "deployer")
        .await
        .expect_err("a dry-run on a backend with no shadow capability must NOT report a fake pass");
    assert!(
        matches!(err, DryRunError::ShadowUnsupported),
        "the honest outcome is the explicit ShadowUnsupported, got: {err:?}"
    );

    // (b) the DECLARATIVE dry-run path is equally honest: a real plan, but the same
    //     explicit ShadowUnsupported (no shadow clone exists to validate against).
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
        runtime_options: Default::default(),
    }];
    let desired = desired_snapshot(PROJECT, &v1, &effective_policy()).expect("v1 desired");
    let plan = engine
        .plan_declarative(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &sqlite_author(),
            &[],
            &guard_cfg(),
            &effective_policy(),
        )
        .expect("plan create");
    let err = engine
        .dry_run_declarative(&be, &plan, &desired, &cfg, &shadow_cfg, "deployer")
        .await
        .expect_err("declarative dry-run must also be explicitly unsupported on SQLite");
    assert!(
        matches!(err, DryRunError::ShadowUnsupported),
        "declarative dry-run yields the explicit ShadowUnsupported, got: {err:?}"
    );
}
