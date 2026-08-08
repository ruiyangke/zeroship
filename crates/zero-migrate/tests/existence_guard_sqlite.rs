//! Faithful e2e for the executor-side existence-guard catalog
//! probe on REAL `SQLite` (temp-file backend). Each test builds a guarded `op.*` IR,
//! lowers it through the REAL `IrAuthor` (`SQLite` dialect, which stamps the
//! `GuardProbe` onto the lowered `Migration`), and applies it through the REAL
//! `SqliteBackend::apply_one` — the same per-migration apply seam
//! `execute_pending` drives under the held lock. No shims.
//!
//! SQLite-supported guarded ops (addConstraint/alterColumn/dropConstraint are
//! PG-only at lower — they route through the rebuild on `SQLite`, so they are not
//! stand-alone-guardable here):
//! - addColumn ifNotExists: absent → runs; present+matching → `SatisfiedNoop` (no
//!   duplicate-column error, version journaled); present+divergent type →
//!   `ExistenceGuardDrift` naming `data_type`, nothing applied.
//! - createTable ifNotExists: present+extra-live-column → fail closed.
//! - dropColumn ifExists: present → drops + journaled; absent → `SatisfiedNoop`.
//! - dropTable ifExists: absent → `SatisfiedNoop` journaled.
//!
//! Plus the `SQLite` affinity-fold compare (F1): the PG-spelled snapshot `data_type` is
//! folded to the `SQLite` affinity (`sqlite_canonical_type`) before compare — exactly
//! as the differ does — so a guarded createTable/addColumn re-run is idempotent for
//! every type (timestamp/jsonb/text/ref all fold to the `text` affinity and match),
//! while a GENUINE affinity change (string→number, text↔real) still fails closed.

mod support;

use std::path::PathBuf;
use tempfile::TempDir;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::ApplyError;
use zero_migrate::apply::journal::Phase;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::ir::{
    ColType, ExistenceGuard, IndexElement, IrColumn, MigrationIr, Op, SelectAst, SelectItem,
    TableRef, ViewQuery,
};
use zero_migrate::model::migration::Migration;
use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{resolve_create_table_policy, SqliteBackend};

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

fn cfg() -> ExecutorConfig {
    ExecutorConfig::new("prj_test", "main", support::no_inject("main"))
}

/// Lower a guarded IR op through the REAL `IrAuthor` (`SQLite` dialect). Returns the
/// lowered migrations (with the `GuardProbe` stamped). The bound project schema is
/// `main` (the `SQLite` implicit target).
#[track_caller]
fn lower(op: Op) -> Vec<Migration> {
    // Each call site represents a separate migration file. Stable production
    // identities are derived from owner + migration name, so the fixture must
    // not reuse one name for unrelated setup and guarded operations.
    let migration_name = format!("guard_test_{}", std::panic::Location::caller().line());
    let ir = MigrationIr {
        ir_version: 2,
        name: migration_name,
        owner_app: "app_test".into(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    let ir = resolve_create_table_policy(&ir, &support::confined_charter(), "main")
        .expect("guard test IR resolves");
    let author = IrAuthor::new(
        "main",
        "app_test",
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    author
        .lower(&ir, &LiveSchema::default())
        .expect("guarded op lowers")
}

async fn apply_one(be: &SqliteBackend, m: &Migration) -> Result<(), ApplyError> {
    be.apply_one(&cfg(), m, "deployer", false, &[], "apply")
        .await
        .map(|_| ())
}

fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.into(),
        ty,
        nullable: Some(true),
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

async fn table_has_column(be: &SqliteBackend, table: &str, column: &str) -> bool {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT name FROM pragma_table_info('{table}') WHERE name = '{column}'"
        ))
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

/// Does an index of this name PHYSICALLY exist on `table`? Probes
/// `PRAGMA index_list(table)` (the `SQLite` analogue of the PG `pg_indexes`
/// helper). `unique_only` further asserts the index is unique.
async fn index_exists(be: &SqliteBackend, table: &str, index: &str, unique_only: bool) -> bool {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT name, \"unique\" FROM pragma_index_list('{table}')"
        ))
        .await
        .expect("pragma index_list");
    rows.iter().any(|r| {
        r.first().and_then(|c| c.as_deref()) == Some(index)
            && (!unique_only || r.get(1).and_then(|c| c.as_deref()) == Some("1"))
    })
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
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }

    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::Text,
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
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
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }
    for m in lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::Text,
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("add the column unguarded");
    }

    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::Text,
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m)
            .await
            .expect("a present TEXT-affinity column is an idempotent satisfied no-op (F1)");
    }
    assert!(
        table_has_column(&be, "t", "email").await,
        "column still present"
    );
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
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
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
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m)
            .await
            .expect("add the integer column unguarded");
    }

    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "count".into(),
        ty: ColType::Int,
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
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
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
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
        ty: ColType::Text,
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m)
            .await
            .expect("add the live string column unguarded");
    }

    // Guarded add declaring a REF (a different SDK facet that also folds to the `text`
    // affinity) → present-match → SatisfiedNoop (NOT a fail-closed, NOT a silent skip
    // over a real divergence — there is none on SQLite).
    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "owner".into(),
        ty: ColType::Ref {
            references: "people".into(),
        },
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m)
            .await
            .expect("a within-TEXT-affinity facet change is an idempotent no-op on SQLite (F1)");
    }
    assert!(
        table_has_column(&be, "t", "owner").await,
        "owner column still present"
    );
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
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
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
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m)
            .await
            .expect("add divergent-type column unguarded");
    }

    // Guarded addColumn declaring text over the live integer column → FailDrift.
    let migs = lower(Op::AddColumn {
        table: "t".into(),
        column: "email".into(),
        ty: ColType::Text,
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let v = migs[0].version.as_str().to_string();
    let err = apply_one(&be, &migs[0])
        .await
        .expect_err("divergent type fails closed");
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
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
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
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    let err = apply_one(&be, &migs[0])
        .await
        .expect_err("wider live table fails closed");
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
        columns: vec![col("legacy", ColType::Text)],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
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
    assert!(
        !table_has_column(&be, "t", "legacy").await,
        "column dropped"
    );
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
        apply_one(&be, m)
            .await
            .expect("drop absent column is a satisfied no-op");
    }
    assert!(journaled(&be, &v2).await, "satisfied no-op journals");
}

/// **F1 regression (the headline finding)** — a guarded `createTable ifNotExists`
/// RE-RUN on `SQLite` must be an idempotent no-op. Before the fix the Table probe's
/// `expect_columns` `data_type` was the PG spelling (`field_data_type` always maps via
/// the PG dialect), so a timestamp system column read as `timestamp with time zone`
/// while the live `SQLite` catalog reports the `text` affinity. `decide_table`'s raw
/// `expect != live` compare therefore hard-FailDrifted (`ExistenceGuardDrift` naming
/// `column t.created_at` / `data_type`) on EVERY re-deploy — every table carries
/// `created_at/updated_at/deleted_at` timestamps, so NO guarded `SQLite` createTable was
/// idempotent. After the fix the decider folds both sides through
/// `sqlite_canonical_type` (timestamp/jsonb/text → `text` affinity) and the Table
/// leg verifies presence+affinity only, so the re-run is a `SatisfiedNoop`.
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
        columns: vec![
            col("title", ColType::Text),
            col("happened", ColType::Timestamp),
        ],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    };

    // First apply: creates the table + secondary indexes.
    let steps1 = lower(make_op());
    for m in &steps1 {
        apply_one(&be, m)
            .await
            .expect("fresh guarded create applies");
    }
    assert!(table_exists(&be, "t").await, "table created");
    assert!(
        table_has_column(&be, "t", "title").await,
        "title column present"
    );
    assert!(
        table_has_column(&be, "t", "happened").await,
        "timestamp column present"
    );

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
    assert!(
        table_has_column(&be, "t", "title").await,
        "title survives re-run"
    );
    assert!(
        table_has_column(&be, "t", "happened").await,
        "timestamp survives re-run"
    );
}

/// **Regression — `SQLite` multi-unit secondary-index physical existence.**
/// A guarded `createTable ifNotExists` with a `unique:true` field lowers to
/// MULTIPLE units on `SQLite` too: the CREATE TABLE (which inlines the system-field
/// indexes) PLUS a SEPARATE `CREATE INDEX` unit for the unique field's index
/// (`lower_create_table` skips only the SYSTEM-field indexes on `SQLite` —
/// declarative.rs:4587 — every other non-PK index, including a `unique:true`
/// field's, is its own guarded `CREATE INDEX` unit; declarative.rs:4583-4604).
///
/// Before the fix the SAME `Table` probe was stamped on EVERY unit, so once
/// unit 0 created the table, the index unit saw the table PRESENT + base columns
/// matching → `SatisfiedNoop` → the unique index was SILENTLY SKIPPED (but journaled
/// completed). This asserts the unique index PHYSICALLY exists (via
/// `PRAGMA index_list`) after a fresh guarded create, and that a RE-RUN is an
/// idempotent no-op with the index surviving — the `SQLite` leg of the fix that
/// the PG `create_table_ifnotexists_fresh_creates_all_secondary_indexes_…` test
/// covers on the PG leg.
///
/// RED pre-fix: the per-unit object-scoped probe does not exist, the index unit
/// `SatisfiedNoops` on the table's presence, so `index_exists(… "t_email_key", …)`
/// is false.
#[compio::test]
async fn create_table_ifnotexists_fresh_creates_unique_secondary_index_and_reruns_idempotent() {
    let p = paths("sq_create_unique_idx");
    let be = backend(&p);

    // a `unique:true` field → a `t_email_key` unique index unit, ON TOP of the
    // CREATE TABLE unit (which inlines the SQLite system-field indexes).
    let make_op = || Op::CreateTable {
        name: "t".into(),
        columns: vec![IrColumn {
            name: "email".into(),
            ty: ColType::Text,
            nullable: Some(true),
            default: None,
            unique: Some(true),
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    };

    // Fresh guarded create on an empty db → > 1 unit (CREATE TABLE + the unique
    // index). The index unit's OWN object-scoped probe sees the index ABSENT →
    // RunBare, not SatisfiedNoop'd by the table's presence.
    let steps1 = lower(make_op());
    assert!(
        steps1.len() >= 2,
        "a guarded SQLite createTable with a unique field lowers to ≥2 units \
         (CREATE TABLE + CREATE INDEX); got {}",
        steps1.len()
    );
    for m in &steps1 {
        apply_one(&be, m)
            .await
            .expect("fresh guarded create applies");
    }
    assert!(table_exists(&be, "t").await, "table created");
    assert!(
        table_has_column(&be, "t", "email").await,
        "email column present"
    );
    assert!(
        index_exists(&be, "t", "t_email_key", true).await,
        "the unique:true field's index must PHYSICALLY exist (C1: it was silently skipped)"
    );
    for m in &steps1 {
        assert!(
            journaled(&be, m.version.as_str()).await,
            "unit {} journaled",
            m.version.as_str()
        );
    }

    // RE-RUN: a fresh lowering of the SAME guarded create over the now-present
    // table+index must be an idempotent no-op — the CREATE TABLE unit SatisfiedNoops
    // (presence + affinity match), the index unit SatisfiedNoops on its OWN object
    // (present + matching unique/columns), with NO "already exists" error and NO
    // ExistenceGuardDrift.
    let steps2 = lower(make_op());
    for m in &steps2 {
        apply_one(&be, m)
            .await
            .expect("re-run of the guarded create is an idempotent no-op");
        assert!(
            journaled(&be, m.version.as_str()).await,
            "re-run unit {} re-journaled (satisfied no-op still journals)",
            m.version.as_str()
        );
    }
    // The unique index physically survives the re-run (not dropped/churned).
    assert!(
        index_exists(&be, "t", "t_email_key", true).await,
        "unique index survives the idempotent re-run"
    );
}

/// **F1 regression — addColumn re-run for a timestamp column.** A guarded
/// `addColumn ifNotExists` of a timestamp column, re-run over the now-present
/// column, must be a `SatisfiedNoop` (not a false `timestamp with time zone != text`
/// drift). The timestamp is NOT a within-text-affinity SDK-facet ambiguity that the
/// Column-leg guards (that is for string↔ref/date authored as a bare string); a
/// timestamp authored as a timestamp folds to the `text` affinity and matches.
#[compio::test]
async fn add_column_ifnotexists_timestamp_rerun_is_noop() {
    let p = paths("sq_add_ts_rerun");
    let be = backend(&p);
    for m in lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("n", ColType::Int)],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
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
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: Some(ExistenceGuard::IfNotExists),
    });
    for m in &migs1 {
        apply_one(&be, m)
            .await
            .expect("guarded addColumn(timestamp) runs");
    }
    assert!(
        table_has_column(&be, "t", "happened").await,
        "timestamp column created"
    );

    // RE-RUN: present + matching affinity → SatisfiedNoop, NOT a false drift.
    let migs2 = lower(Op::AddColumn {
        table: "t".into(),
        column: "happened".into(),
        ty: ColType::Timestamp,
        nullable: Some(true),
        default: None,
        value_format: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
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
        apply_one(&be, m)
            .await
            .expect("drop absent table is a satisfied no-op");
    }
    assert!(!table_exists(&be, "ghost").await);
    assert!(journaled(&be, &v).await, "satisfied no-op journals");
}

#[compio::test]
async fn drop_view_ifexists_present_runs_absent_noops() {
    let p = paths("sq_drop_view");
    let be = backend(&p);

    for m in lower(Op::CreateTable {
        name: "t".into(),
        columns: vec![col("name", ColType::Text)],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],

        partition_by: None,

        runtime_options: None,
        schema: None,
        existence_guard: None,
    }) {
        apply_one(&be, &m).await.expect("create base table");
    }

    for m in lower(Op::CreateView {
        name: "active_v".into(),
        schema: None,
        columns: None,
        query: ViewQuery::Structured {
            select: Box::new(SelectAst {
                from: TableRef {
                    name: "t".into(),
                    schema: None,
                    alias: None,
                },
                projection: vec![SelectItem::ColRef {
                    table: None,
                    name: "name".into(),
                    alias: None,
                }],
                joins: vec![],
                r#where: None,
                group_by: Vec::new(),
                having: None,
                order_by: None,
                limit: None,
            }),
        },
        replace: None,
        materialized: None,
    }) {
        apply_one(&be, &m).await.expect("create view");
    }
    assert!(view_exists(&be, "active_v").await, "view created");

    let migs = lower(Op::DropView {
        name: "active_v".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
        materialized: None,
    });
    let v = migs[0].version.as_str().to_string();
    for m in &migs {
        apply_one(&be, m).await.expect("drop present view runs");
    }
    assert!(!view_exists(&be, "active_v").await, "view dropped");
    assert!(journaled(&be, &v).await, "present drop journals");

    let migs2 = lower(Op::DropView {
        name: "active_v".into(),
        schema: None,
        existence_guard: Some(ExistenceGuard::IfExists),
        materialized: None,
    });
    let v2 = migs2[0].version.as_str().to_string();
    for m in &migs2 {
        apply_one(&be, m)
            .await
            .expect("drop absent view is a satisfied no-op");
    }
    assert!(!view_exists(&be, "active_v").await, "view remains absent");
    assert!(journaled(&be, &v2).await, "satisfied no-op journals");
}

// --- createIndex ifNotExists vs the dialect's index-name SCOPE ---------------

/// A guarded `createIndex` whose name is already taken by an index on ANOTHER
/// table must not be satisfied by that index. SQLite scopes an index name
/// schema-wide, so the name is unavailable and the CREATE cannot succeed: the
/// verdict is a fail-closed drift naming the owning table, never a
/// `SatisfiedNoop` that journals the migration complete while the declared index
/// was never created.
///
/// Does NOT cover MySQL, which scopes index names per table (there the same shape
/// is a plain absent-index `RunBare`) - and which needs no arm here in any case,
/// because the MySQL backend evaluates no existence-guard probe at all. The
/// decider's MySQL contract is pinned by the unit test
/// `index_ownership_only_ignores_a_foreign_owner_where_names_are_per_table` in
/// `render::existence_probe`.
///
/// Does NOT cover the `ifExists` drop path, where a schema-wide name still resolves
/// through the wider scan - AND NOTHING ELSE COVERS IT: no test in this workspace
/// authors a guarded `dropIndex`, and every `GuardProbe::Index` unit test drives
/// `IfNotExists`. A hole, not a handoff.
#[compio::test]
async fn create_index_ifnotexists_name_owned_by_another_table_fails_closed() {
    let p = paths("sq_idx_name_scope");
    let be = backend(&p);

    let make_table = |name: &str| Op::CreateTable {
        name: name.into(),
        columns: vec![col("bucket", ColType::Int)],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    };
    let make_index = |table: &str, guard: Option<ExistenceGuard>| Op::CreateIndex {
        table: table.into(),
        columns: vec![IndexElement::Column {
            name: "bucket".into(),
            order: None,
            opclass: None,
            collation: None,
        }],
        name: Some("idx_shared".into()),
        unique: Some(false),
        using: None,
        r#where: None,
        concurrently: None,
        include: vec![],
        with: None,
        only: None,
        nulls_not_distinct: None,
        schema: None,
        existence_guard: guard,
    };

    // `lower` derives the migration name from the CALL SITE line, so each setup op
    // gets its own `lower` call and therefore its own stable version.
    for m in lower(make_table("a")) {
        apply_one(&be, &m).await.expect("table a applies");
    }
    for m in lower(make_table("b")) {
        apply_one(&be, &m).await.expect("table b applies");
    }
    for m in lower(make_index("a", None)) {
        apply_one(&be, &m).await.expect("a's index applies");
    }
    assert!(
        index_exists(&be, "a", "idx_shared", false).await,
        "table a owns idx_shared before the guarded create"
    );
    assert!(
        !index_exists(&be, "b", "idx_shared", false).await,
        "table b has no idx_shared before the guarded create"
    );

    let guarded = lower(make_index("b", Some(ExistenceGuard::IfNotExists)));
    let mut versions = Vec::new();
    for m in &guarded {
        versions.push(m.version.as_str().to_string());
        let err = apply_one(&be, m)
            .await
            .expect_err("a name owned by another table must not be a satisfied no-op");
        let (object, field) = expect_drift(err);
        assert_eq!(object, "index idx_shared");
        assert_eq!(field, "table");
    }
    assert!(
        !index_exists(&be, "b", "idx_shared", false).await,
        "the refused create left b untouched"
    );
    assert!(
        index_exists(&be, "a", "idx_shared", false).await,
        "the refused create left a's index intact"
    );
    for version in &versions {
        assert!(
            !journaled(&be, version).await,
            "a refused guarded create journals nothing"
        );
    }
}

/// An UNGUARDED `createIndex` naming an index another table owns must fail closed
/// too, on the SQLite leg.
///
/// The SQLite emitter writes `CREATE INDEX IF NOT EXISTS` whether or not the author
/// asked, and SQLite scopes an index name across the whole `main` schema. So the
/// create is skipped by the engine, the migration is journaled complete, and the
/// declared index never exists: the same silent skip the guarded path already
/// refuses, with no author guard behind it. The control in the same test is the free
/// name, which must still reach the CREATE.
///
/// Does NOT cover MySQL (per-table index names), where there is nothing to cover:
/// the MySQL emitter writes no `IF NOT EXISTS` and the MySQL backend evaluates no
/// probe, so the silent-skip shape this arm pins cannot occur there.
///
/// Does NOT cover a shape divergence - by design, not by omission: the unguarded
/// decision is ownership and nothing else, and the GUARDED path
/// (`create_index_ifnotexists_name_owned_by_another_table_fails_closed` above, plus
/// the `index_ifnotexists_*` unit tests in `render::existence_probe`) owns shape.
///
/// Does NOT cover a collision the same migration UNIT creates before the statement
/// runs, AND NOTHING ELSE COVERS IT: the probe reads one catalog snapshot per unit,
/// and the fold's `DuplicateIndex` check keys on the target table's own index list,
/// so it never asks which OTHER table owns a name. A hole, not a handoff.
#[compio::test]
async fn create_index_unguarded_name_owned_by_another_table_fails_closed() {
    let p = paths("sq_idx_bare_scope");
    let be = backend(&p);

    let make_table = |name: &str| Op::CreateTable {
        name: name.into(),
        columns: vec![col("bucket", ColType::Int)],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    };
    let make_index = |table: &str, index: &str| Op::CreateIndex {
        table: table.into(),
        columns: vec![IndexElement::Column {
            name: "bucket".into(),
            order: None,
            opclass: None,
            collation: None,
        }],
        name: Some(index.into()),
        unique: Some(false),
        using: None,
        r#where: None,
        concurrently: None,
        include: vec![],
        with: None,
        only: None,
        nulls_not_distinct: None,
        schema: None,
        // No guard: the author wrote a plain `createIndex`.
        existence_guard: None,
    };

    // `lower` derives the migration name from the CALL SITE line, so each setup op
    // gets its own `lower` call and therefore its own stable version.
    for m in lower(make_table("a")) {
        apply_one(&be, &m).await.expect("table a applies");
    }
    for m in lower(make_table("b")) {
        apply_one(&be, &m).await.expect("table b applies");
    }
    for m in lower(make_index("a", "idx_shared")) {
        apply_one(&be, &m).await.expect("a's index applies");
    }
    assert!(
        index_exists(&be, "a", "idx_shared", false).await,
        "table a owns idx_shared before the unguarded create"
    );

    let collide = lower(make_index("b", "idx_shared"));
    let mut versions = Vec::new();
    for m in &collide {
        versions.push(m.version.as_str().to_string());
        let err = apply_one(&be, m)
            .await
            .expect_err("a name owned by another table must not be a silent skip");
        let (object, field) = expect_drift(err);
        assert_eq!(object, "index idx_shared");
        assert_eq!(field, "table");
    }
    assert!(
        !index_exists(&be, "b", "idx_shared", false).await,
        "the refused create left b untouched"
    );
    for version in &versions {
        assert!(
            !journaled(&be, version).await,
            "a refused unguarded create journals nothing, so its name can still be fixed"
        );
    }

    // CONTROL: the only variable is the index name. A free name still lands on b.
    let free = lower(make_index("b", "idx_free"));
    for m in &free {
        apply_one(&be, m).await.expect("a free name still creates");
    }
    assert!(
        index_exists(&be, "b", "idx_free", false).await,
        "an unclaimed name still reaches the CREATE"
    );

    // CONTROL: re-running the SAME unguarded create is the `IF NOT EXISTS` no-op
    // that non-transactional crash recovery replays. The probe decides ownership
    // only, so the index b already owns is b's own and the statement runs bare; a
    // shape-verifying probe would fail closed here on the absent expectation.
    for m in &free {
        apply_one(&be, m)
            .await
            .expect("re-running an unguarded create over its own index stays a no-op");
    }
    assert!(
        index_exists(&be, "b", "idx_free", false).await,
        "the re-run left b's index in place"
    );
}
