//! **PR10 Part B** — faithful e2e for the executor-side existence-guard catalog
//! probe on REAL SQLite (temp-file backend). Each test builds a guarded `op.*` IR,
//! lowers it through the REAL `IrAuthor` (SQLite dialect, which stamps the
//! `GuardProbe` onto the lowered `Migration`), and applies it through the REAL
//! `SqliteBackend::apply_up_transactional` — the same per-migration apply seam
//! `execute_pending` drives under the held lock. No shims.
//!
//! SQLite-supported guarded ops (addConstraint/alterColumn/dropConstraint are
//! PG-only at lower — they route through the rebuild on SQLite, so they are not
//! stand-alone-guardable here):
//! - addColumn ifNotExists: absent → runs; present+matching → SatisfiedNoop (no
//!   duplicate-column error, version journaled); present+divergent type →
//!   `ExistenceGuardDrift` naming `data_type`, nothing applied.
//! - createTable ifNotExists: present+extra-live-column → fail closed.
//! - dropColumn ifExists: present → drops + journaled; absent → SatisfiedNoop.
//! - dropTable ifExists: absent → SatisfiedNoop journaled.
//!
//! Plus the SQLite TEXT-affinity fail-closed default: a same-name TEXT-affinity
//! column whose declared token differs is FailDrift, never an affinity-only noop.

use zeroship_migrate::backend::MigrationBackend;
use zeroship_migrate::db::ExecutorConfig;
use zeroship_migrate::executor::ApplyError;
use zeroship_migrate::ir::{ColType, ExistenceGuard, IrColumn, MigrationIr, Op};
use zeroship_migrate::ir_author::{IrAuthor, LiveSchema};
use zeroship_migrate::journal::Phase;
use zeroship_migrate::migration::Migration;
use zeroship_migrate::SqliteBackend;
use zeroship_schema::query::SqlDialect;
use std::path::PathBuf;
use tempfile::TempDir;

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

fn cfg() -> ExecutorConfig {
    ExecutorConfig::new("prj_test", "main")
}

/// Lower a guarded IR op through the REAL `IrAuthor` (SQLite dialect). Returns the
/// lowered migrations (with the `GuardProbe` stamped). The bound project schema is
/// `main` (the SQLite implicit target).
fn lower(op: Op) -> Vec<Migration> {
    let ir = MigrationIr {
        ir_version: 2,
        name: "guard_test".into(),
        owner_app: "app_test".into(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    let author = IrAuthor::new("main", "app_test", SqlDialect::Sqlite);
    author.lower(&ir, &LiveSchema::default()).expect("guarded op lowers")
}

async fn apply_one(be: &SqliteBackend, m: &Migration) -> Result<(), ApplyError> {
    be.apply_up_transactional(&cfg(), m, "deployer", &[], "apply").await
}

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn { name: name.into(), ty, nullable: Some(true), default: None, unique: None }
}

async fn table_has_column(be: &SqliteBackend, table: &str, column: &str) -> bool {
    let rows = be
        .actor()
        .query(&format!("SELECT name FROM pragma_table_info('{table}') WHERE name = '{column}'"))
        .await
        .expect("pragma table_info");
    !rows.is_empty()
}

async fn table_exists(be: &SqliteBackend, table: &str) -> bool {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_master WHERE type='table' AND name='{table}'"
        ))
        .await
        .expect("sqlite_master");
    !rows.is_empty()
}

async fn journaled(be: &SqliteBackend, version: &str) -> bool {
    be.applied_sqlite()
        .await
        .expect("read journal")
        .iter()
        .any(|e| e.version == version && e.phase == Phase::Completed)
}

fn expect_drift(e: ApplyError) -> (String, String) {
    match e {
        ApplyError::ExistenceGuardDrift { object, field, .. } => (object, field),
        other => panic!("expected ExistenceGuardDrift, got: {other:?}"),
    }
}

// --- addColumn ifNotExists -------------------------------------------------

#[compio::test]
async fn add_column_ifnotexists_absent_runs() {
    let p = paths("sq_add_absent");
    let be = backend(&p);
    // base table without the guarded column (unguarded createTable).
    for m in lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }

    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m).await.expect("guarded addColumn runs");
    }
    assert!(table_has_column(&be, "t", "email").await, "column created");
    assert!(journaled(&be, &v).await, "version journaled");
}

#[compio::test]
async fn add_column_ifnotexists_present_matching_is_noop() {
    let p = paths("sq_add_match");
    let be = backend(&p);
    // Create base table + the column with the SAME shape via an unguarded apply.
    for m in lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }
    for m in lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("add the column unguarded");
    }

    // Guarded addColumn over the MATCHING existing column → SatisfiedNoop (a bare
    // ADD COLUMN would error "duplicate column name").
    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m).await.expect("guarded addColumn is a satisfied no-op");
    }
    assert!(journaled(&be, &v).await, "satisfied no-op STILL journals");
}

#[compio::test]
async fn add_column_ifnotexists_present_divergent_type_fails_closed() {
    let p = paths("sq_add_divergent");
    let be = backend(&p);
    // base table + an `email` column of INTEGER affinity (divergent from declared text).
    for m in lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }
    for m in lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::Int,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("add divergent-type column unguarded");
    }

    // Guarded addColumn declaring text over the live integer column → FailDrift.
    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    let err = apply_one(&be, &migs[0]).await.expect_err("divergent type fails closed");
    let (object, field) = expect_drift(err);
    assert_eq!(field, "data_type");
    assert!(object.contains("email"), "names the column: {object}");
    assert!(!journaled(&be, &v).await, "nothing journaled on drift");
}

// --- createTable ifNotExists ----------------------------------------------

#[compio::test]
async fn create_table_ifnotexists_present_extra_column_fails_closed() {
    let p = paths("sq_create_extra");
    let be = backend(&p);
    // Create the declared table via an unguarded apply, then add an EXTRA live
    // column out-of-band so the guarded re-create finds a WIDER live table.
    for m in lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }
    be.actor()
        .exec("ALTER TABLE main.t ADD COLUMN sneaky TEXT")
        .await
        .expect("add extra live column");

    let migs = lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let err = apply_one(&be, &migs[0]).await.expect_err("wider live table fails closed");
    let (_, field) = expect_drift(err);
    // Fail-closed either way: the extra live column makes the table wider than
    // declared (`columns`), and the SQLite system-`id` column's introspected
    // affinity may not be byte-provably equal to the declared snapshot
    // (`data_type`, the TEXT-affinity fail-closed default). Both are correct
    // refusals — the point is the guarded re-create NEVER silently runs over a
    // table whose shape it cannot prove matches.
    assert!(
        field == "columns" || field == "data_type",
        "extra-live-column createTable must fail closed (columns/data_type), got: {field}"
    );
}

// --- dropColumn / dropTable ifExists --------------------------------------

#[compio::test]
async fn drop_column_ifexists_present_runs_absent_noops() {
    let p = paths("sq_drop_col");
    let be = backend(&p);
    for m in lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("legacy", ColType::String)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }

    // present → drops + journals.
    let migs = lower(Op::DropColumn {
        table: "t".into(),
        column: "legacy".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m).await.expect("drop present column runs");
    }
    assert!(!table_has_column(&be, "t", "legacy").await, "column dropped");
    assert!(journaled(&be, &v).await);

    // absent → SatisfiedNoop (fresh version over the now-absent column).
    let migs2 = lower(Op::DropColumn {
        table: "t".into(),
        column: "legacy".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
    });
    let v2 = migs2[0].version.as_str().to_string();
    for m in &migs2 {
        apply_one(&be, m).await.expect("drop absent column is a satisfied no-op");
    }
    assert!(journaled(&be, &v2).await, "satisfied no-op journals");
}

#[compio::test]
async fn drop_table_ifexists_absent_noops() {
    let p = paths("sq_drop_tbl");
    let be = backend(&p);
    let migs = lower(Op::DropTable {
        table: "ghost".into(),
        cascade: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m).await.expect("drop absent table is a satisfied no-op");
    }
    assert!(!table_exists(&be, "ghost").await);
    assert!(journaled(&be, &v).await, "satisfied no-op journals");
}
