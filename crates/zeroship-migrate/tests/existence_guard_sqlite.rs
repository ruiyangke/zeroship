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
//! Plus the SQLite affinity-fold compare (F1): the PG-spelled snapshot data_type is
//! folded to the SQLite affinity (`sqlite_canonical_type`) before compare — exactly
//! as the differ does — so a guarded createTable/addColumn re-run is idempotent for
//! every type (timestamp/jsonb/text/ref all fold to the `text` affinity and match),
//! while a GENUINE affinity change (string→number, text↔real) still fails closed.

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
async fn add_column_ifnotexists_present_text_affinity_match_is_noop() {
    // **F1 — SQLite text-affinity present-match is an idempotent no-op** (restores the
    // noop coverage finding 4 flagged). On SQLite a present TEXT-affinity column reads
    // back as the `text` affinity; a same-token `string`-over-`string` guarded re-add
    // folds to `text` == `text` → SatisfiedNoop (NOT a duplicate-column error, NOT a
    // fail-closed). This matches the declarative DIFFER, which compares only the
    // canonical affinity on SQLite (the within-affinity facet blind spot is a
    // documented SQLite divergence — and a `string`/`ref` column is physically the
    // same `text` column on SQLite either way).
    let p = paths("sq_add_text_match");
    let be = backend(&p);
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
        apply_one(&be, m)
            .await
            .expect("a present TEXT-affinity column is an idempotent satisfied no-op (F1)");
    }
    assert!(table_has_column(&be, "t", "email").await, "column still present");
    assert!(journaled(&be, &v).await, "satisfied no-op still journals");
}

#[compio::test]
async fn add_column_ifnotexists_present_integer_affinity_match_is_noop() {
    // The genuine SatisfiedNoop case on SQLite: a NON-TEXT (INTEGER) affinity is
    // UNAMBIGUOUS — a present `int` column over a declared `int` add is provably
    // equal, so it is a satisfied no-op (no fail-closed, version journaled).
    let p = paths("sq_add_int_match");
    let be = backend(&p);
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
        column: "count".into(),
        ty: ColType::Int,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("add the integer column unguarded");
    }

    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "count".into(),
        ty: ColType::Int,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m)
            .await
            .expect("INTEGER-affinity match is a provable satisfied no-op");
    }
    assert!(journaled(&be, &v).await, "satisfied no-op STILL journals");
}

#[compio::test]
async fn add_column_ifnotexists_sqlite_ref_over_live_string_is_noop() {
    // **F1 — within-TEXT-affinity facet change is a no-op on SQLite (differ-consistent).**
    // addColumn ifNotExists declaring a `ref` over a column the live DB authored as
    // `string`. BOTH `string` and `ref` fold to the SQLite `text` affinity, and on
    // SQLite a `ref` column adds NO foreign key via `ALTER` — it is physically the
    // SAME `text` column either way, so the facet difference carries no provable
    // physical divergence. The declarative DIFFER treats this as no-change (it compares
    // only the canonical affinity), and the guard now matches: a SatisfiedNoop, so a
    // guarded re-add is idempotent. (A GENUINE affinity change — string→number — still
    // diverges; see `add_column_ifnotexists_present_divergent_type_fails_closed`.)
    let p = paths("sq_add_ref_over_string");
    let be = backend(&p);
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
    // The live column is authored as a plain STRING (→ snapshot `text` → SQLite TEXT
    // affinity).
    for m in lower(Op::AddColumn {
        table: "t".into(),
        column: "owner".into(),
        ty: ColType::String,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("add the live string column unguarded");
    }

    // Guarded add declaring a REF (a different SDK facet that also folds to the `text`
    // affinity) → present-match → SatisfiedNoop (NOT a fail-closed, NOT a silent skip
    // over a real divergence — there is none on SQLite).
    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "owner".into(),
        ty: ColType::Ref { references: "people".into() },
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m)
            .await
            .expect("a within-TEXT-affinity facet change is an idempotent no-op on SQLite (F1)");
    }
    assert!(table_has_column(&be, "t", "owner").await, "owner column still present");
    assert!(journaled(&be, &v).await, "satisfied no-op still journals");
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

/// **F1 regression (the headline finding)** — a guarded `createTable ifNotExists`
/// RE-RUN on SQLite must be an idempotent no-op. Before the fix the Table probe's
/// `expect_columns` data_type was the PG spelling (`field_data_type` always maps via
/// the PG dialect), so a timestamp system column read as `timestamp with time zone`
/// while the live SQLite catalog reports the `text` affinity. `decide_table`'s raw
/// `expect != live` compare therefore hard-FailDrifted (`ExistenceGuardDrift` naming
/// `column t.created_at` / `data_type`) on EVERY re-deploy — every table carries
/// created_at/updated_at/deleted_at timestamps, so NO guarded SQLite createTable was
/// idempotent. After the fix the decider folds both sides through
/// `sqlite_canonical_type` (timestamp/jsonb/text → `text` affinity) and the Table
/// leg verifies presence+affinity only, so the re-run is a SatisfiedNoop.
///
/// RED pre-fix: the second apply returns `ExistenceGuardDrift { data_type }`.
#[compio::test]
async fn create_table_ifnotexists_reruns_idempotent_with_timestamp_and_text_columns() {
    let p = paths("sq_create_rerun");
    let be = backend(&p);
    // A table with a text column AND a timestamp column ON TOP of the always-present
    // system fields (id text, created_at/updated_at/deleted_at timestamps, …).
    let make_op = || Op::CreateTable {
        name: "t".into(),
        columns: vec![col("title", ColType::String), col("happened", ColType::Timestamp)],
        constraints: vec![],
        indexes: vec![],
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    };

    // First apply: creates the table + secondary indexes.
    let steps1 = lower(make_op());
    for m in &steps1 {
        apply_one(&be, m).await.expect("fresh guarded create applies");
    }
    assert!(table_exists(&be, "t").await, "table created");
    assert!(table_has_column(&be, "t", "title").await, "title column present");
    assert!(table_has_column(&be, "t", "happened").await, "timestamp column present");

    // RE-RUN: a fresh lowering of the SAME guarded create over the now-present table
    // must be an idempotent no-op — the CREATE TABLE unit SatisfiedNoops (presence +
    // affinity match across the text/timestamp/system columns), every index unit
    // SatisfiedNoops on its own object. NO ExistenceGuardDrift.
    let steps2 = lower(make_op());
    for m in &steps2 {
        apply_one(&be, m)
            .await
            .expect("re-run of the guarded createTable is an idempotent no-op (F1)");
    }
    // Every re-run unit re-journaled (satisfied no-op still journals).
    for m in &steps2 {
        assert!(
            journaled(&be, m.version.as_str()).await,
            "re-run unit {} re-journaled",
            m.version.as_str()
        );
    }
    // The columns physically survive the re-run (nothing churned/dropped).
    assert!(table_has_column(&be, "t", "title").await, "title survives re-run");
    assert!(table_has_column(&be, "t", "happened").await, "timestamp survives re-run");
}

/// **F1 regression — addColumn re-run for a timestamp column.** A guarded
/// `addColumn ifNotExists` of a timestamp column, re-run over the now-present
/// column, must be a SatisfiedNoop (not a false `timestamp with time zone != text`
/// drift). The timestamp is NOT a within-text-affinity SDK-facet ambiguity that the
/// Column-leg H1 guards (that is for string↔ref/date authored as a bare string); a
/// timestamp authored as a timestamp folds to the `text` affinity and matches.
#[compio::test]
async fn add_column_ifnotexists_timestamp_rerun_is_noop() {
    let p = paths("sq_add_ts_rerun");
    let be = backend(&p);
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
    // First guarded add: runs (absent → creates the timestamp column).
    let migs1 = lower(Op::AddColumn {
        table: "t".into(),
        column: "happened".into(),
        ty: ColType::Timestamp,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    for m in &migs1 {
        apply_one(&be, m).await.expect("guarded addColumn(timestamp) runs");
    }
    assert!(table_has_column(&be, "t", "happened").await, "timestamp column created");

    // RE-RUN: present + matching affinity → SatisfiedNoop, NOT a false drift.
    let migs2 = lower(Op::AddColumn {
        table: "t".into(),
        column: "happened".into(),
        ty: ColType::Timestamp,
        nullable: Some(true),
        default: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v2 = migs2[0].version.as_str().to_string();
    for m in &migs2 {
        apply_one(&be, m)
            .await
            .expect("re-run of guarded addColumn(timestamp) is an idempotent no-op (F1)");
    }
    assert!(journaled(&be, &v2).await, "satisfied no-op still journals");
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
