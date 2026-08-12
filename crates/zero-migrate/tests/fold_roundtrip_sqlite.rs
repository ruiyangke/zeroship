//! **The ROUND-TRIP ORACLE for `fold_ops` on real `SQLite`.**
//!
//! The `SQLite` leg of the fold oracle. Restricted to the ops the `SQLite` backend
//! supports through `IrAuthor::load_and_lower` + `engine.apply` WITHOUT the 12-step
//! rebuild: `createTable` (plain columns + an index), `addColumn`, `dropColumn`,
//! `createIndex`, `dropIndex`. The PG-only ops (`alterColumn*`, stand-alone
//! `addConstraint`, table-level UNIQUE/FK on a `createTable`) are `SqliteRebuildOnly`
//! / not threaded into the `SQLite` emitter, so they live in the PG-only oracle.
//!
//! Same shape as the PG oracle: APPLY the corpus to a real temp-file `SQLite` backend,
//! INTROSPECT via `snapshot_schema_sqlite`, FOLD the SAME ops offline with the
//! `SqlDialect::Sqlite` dialect, assert structural equality. No DB env gate is
//! needed - `SQLite` is an embedded temp file, always available.
//!
//! The comparison runs after EVERY stage, not once at the end of the corpus. A
//! create and a drop of the same object cancel in the folded snapshot, so a single
//! trailing comparison would observe neither half of the `notes_tag_idx` pair.
//!
//! Run with `--test-threads=1` for parity with the rest of the suite.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::{
    apply::executor::LockMode, fold_ops, model::ir::Op, resolve_create_table_policy,
    sqlite_canonical_type, Approval, ExecutorConfig, IrAuthor, LiveSchema, MigrationEngine,
    MigrationIr, SchemaSnapshot, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_fold";
const APP: &str = "app_fold";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT))
}

fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(t, o)| (t.to_string(), o.to_string()))
        .collect()
}

/// Apply one IR doc through the REAL `SQLite` pipeline (`load_and_lower` + engine
/// apply), returning its ordered `Op` list so the caller accumulates the full
/// stream the fold replays. The live table set is threaded so the lower can inline.
async fn apply_doc(
    be: &SqliteBackend,
    ir: &str,
    reg: &BTreeMap<String, String>,
    live_tables: &BTreeSet<String>,
    approval: Approval,
) -> Vec<Op> {
    let raw: MigrationIr = serde_json::from_str(ir).expect("test IR parses");
    let resolved = resolve_create_table_policy(&raw, &support::confined_charter(), PROJECT)
        .expect("test IR resolves");
    let ir = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let document = zero_migrate::model::load::load_ir_document(
        &ir,
        APP,
        zero_migrate::model::validate::Dialect::Sqlite,
        reg,
        None,
    )
    .expect("load gate (sqlite)");
    let ops = document.ops.clone();
    let live = LiveSchema::from_tables(live_tables.clone());
    let plan = author
        .lower_plan(&document, &live)
        .expect("lower the doc plan on SQLite");
    let engine = MigrationEngine::new();
    engine
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
    ops
}

/// SQLite-leg canonicalization to a common comparison form (the SAME normalization
/// the differ's `SQLite` drift comparison uses — `sqlite_canonical_type`):
///
///   1. **Column `data_type` → `SQLite` affinity.** `fold_ops` routes through the
///      shared `build_table_snapshot`, which always emits the PG `information_schema`
///      spelling (`text`/`boolean`/`timestamp with time zone`/`double precision`/…)
///      regardless of dialect, whereas the live `SQLite` catalog reports the `SQLite`
///      declared affinity (`text`/`integer`/`real`/…). Folding BOTH sides through
///      `sqlite_canonical_type` (the exact differ canonicalizer) collapses the
///      spelling divergence while still detecting a REAL type change (string→number
///      maps to two distinct tokens). This is the documented SQLite-introspection
///      normalization the brief calls out.
///   2. **Drop the PRIMARY KEY constraint + its implicit index.** `SQLite` reports the
///      PK constraint under a different introspection NAME (`pk_<table>` vs the fold's
///      `<table>_pkey`) and materializes NO separate index in `sqlite_master`,
///      whereas the fold (PG-shaped) carries `<table>_pkey` for both. The PK is
///      platform-owned and the `id` column itself still compares, so blanking the PK
///      constraint + its same-named index on both sides is introspection-only noise.
///   3. **CHECK `definition` blanked - DEAD on both sides today.** Neither half of
///      this oracle can produce a `CHECK` constraint, so the loop below never fires.
///      `fold_ops` under `SqlDialect::Sqlite` REFUSES one outright: both
///      `fold_create_table_specs` and the `Op::AddConstraint` arm return
///      `FoldError::Unsupported` ("createTable table-level CHECK is PostgreSQL-only"
///      / "addConstraint(check) is PostgreSQL-only"), so a corpus that authored a
///      CHECK would panic in `assert_matches_live` at the fold, never reaching this
///      function. On the other side `snapshot_schema_sqlite` only ever pushes
///      `PRIMARY KEY`, `UNIQUE` and `FOREIGN KEY` into the constraint bucket.
///      Measured: a table created with `CONSTRAINT "parts_qty_check" CHECK (("qty"
///      >= 0))` introspects to `pk_parts` and nothing else, with the CHECK text
///      surviving only on the excluded-from-equality `stored_create_sql`. The
///      current corpus authors no CHECK either, but that is the weaker fact.
///
///      Not parity with the PG oracle in MECHANISM. `fold_roundtrip_pg.rs` (which
///      does exercise a CHECK, at its `add check constraint` checkpoint) compares
///      with `diff_snapshots(...).is_clean()` and inherits the exemption from the
///      differ's `constraint_definition_is_comparable`. This oracle compares
///      snapshots directly, and `ConstraintSnapshot::PartialEq` DOES compare
///      `definition`, so the two exemptions are independent and this one has to be
///      applied by hand.
///
///      The PG REASON for the exemption does not transfer either. PostgreSQL
///      re-deparses through `pg_get_constraintdef`, so the fold's quoted
///      `CHECK (("qty" >= 0))` reads back unquoted. SQLite stores the `CREATE TABLE`
///      text verbatim in `sqlite_master`, and the same body reads back
///      byte-identical, so a SQLite CHECK that reached comparison would need no text
///      exemption for a re-deparse that does not happen.
///
///      Reviving this needs the two fold refusals lifted AND `snapshot_schema_sqlite`
///      taught to surface CHECK constraints. Blanking `definition` alone would not
///      be enough after only the first: a fold-emitted CHECK with no live counterpart
///      is a MISSING constraint, which blanking a field cannot reconcile. Kept rather
///      than deleted so the exemption is in place if SQLite CHECK folding lands
///      before the introspection half.
fn canonicalize(mut snap: SchemaSnapshot) -> SchemaSnapshot {
    for t in snap.tables.values_mut() {
        for c in &mut t.columns {
            c.data_type = sqlite_canonical_type(&c.data_type).to_string();
        }
        // Drop every PRIMARY KEY constraint + its implicit same-named index.
        let pk_names: Vec<String> = t
            .constraints
            .iter()
            .filter(|c| c.kind == "PRIMARY KEY")
            .map(|c| c.name.clone())
            .collect();
        t.constraints.retain(|c| c.kind != "PRIMARY KEY");
        t.indexes.retain(|i| !pk_names.contains(&i.name));
        for c in &mut t.constraints {
            if c.kind == "CHECK" {
                c.definition = String::new();
            }
        }
    }
    snap
}

/// Fold the ops applied so far and compare them against live introspection.
///
/// Called after every stage rather than once at the end of the corpus. A create
/// and a drop of the same object cancel in the folded snapshot, so a single
/// trailing comparison observes neither: `notes_tag_idx` is created in stage 4
/// and dropped in stage 5, and folding both at once yields the same snapshot a
/// fold that ignored `createIndex` and `dropIndex` entirely would yield.
async fn assert_matches_live(be: &SqliteBackend, ops: &[Op], stage: &str) {
    let live = be
        .snapshot_schema_sqlite()
        .await
        .unwrap_or_else(|error| panic!("{stage}: introspect live SQLite schema: {error}"));
    let folded = fold_ops(
        ops,
        SqlDialect::Sqlite,
        PROJECT,
        &support::confined_charter(),
    )
    .unwrap_or_else(|error| panic!("{stage}: fold the corpus offline: {error}"));

    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "{stage}: fold_ops(corpus) must equal the live introspected SQLite snapshot"
    );
}

#[compio::test]
async fn fold_equals_introspect_sqlite() {
    let p = paths("fold_rt");
    let be = backend(&p);

    let mut all_ops: Vec<Op> = Vec::new();
    let mut live_tables: BTreeSet<String> = BTreeSet::new();

    // (1) createTable with plain columns of varied types + an extra index.
    let notes = r#"{"ir_version":1,"name":"create_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false},
            {"name":"body","type":"text","nullable":true},
            {"name":"rank","type":"int","nullable":true},
            {"name":"score","type":"double","nullable":true},
            {"name":"done","type":"boolean","nullable":false}
        ],
        "indexes":[
            {"name":"notes_rank_idx","columns":[{"kind":"column","name":"rank"}]}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&be, notes, &registry(&[]), &live_tables, Approval::None).await);
    live_tables.insert("notes".to_string());
    assert_matches_live(&be, &all_ops, "create table").await;

    let full = registry(&[("notes", APP)]);

    // (2) addColumn.
    let add_col = r#"{"ir_version":1,"name":"add_col","ops":[
        {"op":"addColumn","table":"notes","column":"tag","type":"text","nullable":true}
    ]}"#;
    all_ops.extend(apply_doc(&be, add_col, &full, &live_tables, Approval::None).await);
    assert_matches_live(&be, &all_ops, "add column").await;

    // (3) dropColumn.
    let drop_col = r#"{"ir_version":1,"name":"drop_col","ops":[
        {"op":"dropColumn","table":"notes","column":"score"}
    ]}"#;
    all_ops.extend(apply_doc(&be, drop_col, &full, &live_tables, Approval::Approved).await);
    assert_matches_live(&be, &all_ops, "drop column").await;

    // (4) createIndex, then dropIndex it. Each is compared before the next runs,
    // so neither can be cancelled by its counterpart before anything observes it.
    let mk_idx = r#"{"ir_version":1,"name":"mk_idx","ops":[
        {"op":"createIndex","table":"notes","columns":[{"kind":"column","name":"tag"}],"name":"notes_tag_idx"}
    ]}"#;
    all_ops.extend(apply_doc(&be, mk_idx, &full, &live_tables, Approval::None).await);
    assert_matches_live(&be, &all_ops, "create index").await;

    let drop_idx = r#"{"ir_version":1,"name":"drop_idx","ops":[
        {"op":"dropIndex","name":"notes_tag_idx","table":"notes"}
    ]}"#;
    all_ops.extend(apply_doc(&be, drop_idx, &full, &live_tables, Approval::None).await);
    assert_matches_live(&be, &all_ops, "drop index").await;

    // (5) A case-insensitive text column. SQLite carries the facet as an inline
    // `COLLATE NOCASE` on the column type, and introspection recovers it by reading
    // the stored CREATE TABLE text back, so this stage compares a facet that only
    // round-trips if the emitter and the recovery agree on one spelling.
    let folded = r#"{"ir_version":1,"name":"create_folded","ops":[
        {"op":"createTable","name":"folded","columns":[
            {"name":"email","type":"text","nullable":false,"caseSensitive":false},
            {"name":"handle","type":"text","nullable":true},
            {"name":"rank","type":"int","nullable":true}
        ]}
    ]}"#;
    all_ops.extend(apply_doc(&be, folded, &registry(&[]), &live_tables, Approval::None).await);
    live_tables.insert("folded".to_string());
    assert_matches_live(&be, &all_ops, "create table with a case-insensitive column").await;

    let both = registry(&[("notes", APP), ("folded", APP)]);

    // (6) The index facets SQLite stores in its own CREATE INDEX text: UNIQUE, a
    // partial predicate, an expression key, and a DESC element. Each is compared
    // before the next runs.
    let unique_idx = r#"{"ir_version":1,"name":"unique_idx","ops":[
        {"op":"createIndex","table":"folded","name":"folded_email_key",
         "columns":[{"kind":"column","name":"email"}],"unique":true}
    ]}"#;
    all_ops.extend(apply_doc(&be, unique_idx, &both, &live_tables, Approval::None).await);
    assert_matches_live(&be, &all_ops, "create unique index").await;

    let partial_idx = r#"{"ir_version":1,"name":"partial_idx","ops":[
        {"op":"createIndex","table":"folded","name":"folded_handle_partial_idx",
         "columns":[{"kind":"column","name":"handle"}],
         "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"rank"},
                  "rhs":{"node":"literal","value":0}}}
    ]}"#;
    all_ops.extend(apply_doc(&be, partial_idx, &both, &live_tables, Approval::None).await);
    assert_matches_live(&be, &all_ops, "create partial index").await;

    let expr_idx = r#"{"ir_version":1,"name":"expr_idx","ops":[
        {"op":"createIndex","table":"folded","name":"folded_rank_expr_idx",
         "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
           "lhs":{"node":"colRef","name":"rank"},"rhs":{"node":"literal","value":1}}}]}
    ]}"#;
    all_ops.extend(apply_doc(&be, expr_idx, &both, &live_tables, Approval::None).await);
    assert_matches_live(&be, &all_ops, "create expression index").await;

    let desc_idx = r#"{"ir_version":1,"name":"desc_idx","ops":[
        {"op":"createIndex","table":"folded","name":"folded_handle_desc_idx",
         "columns":[{"kind":"column","name":"handle","order":"desc"}]}
    ]}"#;
    all_ops.extend(apply_doc(&be, desc_idx, &both, &live_tables, Approval::None).await);
    assert_matches_live(&be, &all_ops, "create descending index").await;
}
