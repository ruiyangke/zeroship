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
//! needed — `SQLite` is an embedded temp file, always available.
//!
//! Run with `--test-threads=1` for parity with the rest of the suite.

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
    ExecutorConfig::new(PROJECT, PROJECT)
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
    let resolved =
        resolve_create_table_policy(&raw, &zero_migrate::zeroship_confined_ceiling()).expect("test IR resolves");
    let ir = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zero_migrate::model::load::load_ir_document(
        &ir,
        APP,
        zero_migrate::model::validate::Dialect::Sqlite,
        reg,
        None,
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
///   3. **CHECK `definition` blanked** (parity with the PG oracle; the corpus has
///      none).
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

    let full = registry(&[("notes", APP)]);

    // (2) addColumn.
    let add_col = r#"{"ir_version":1,"name":"add_col","ops":[
        {"op":"addColumn","table":"notes","column":"tag","type":"text","nullable":true}
    ]}"#;
    all_ops.extend(apply_doc(&be, add_col, &full, &live_tables, Approval::None).await);

    // (3) dropColumn.
    let drop_col = r#"{"ir_version":1,"name":"drop_col","ops":[
        {"op":"dropColumn","table":"notes","column":"score"}
    ]}"#;
    all_ops.extend(apply_doc(&be, drop_col, &full, &live_tables, Approval::Approved).await);

    // (4) createIndex, then dropIndex it.
    let mk_idx = r#"{"ir_version":1,"name":"mk_idx","ops":[
        {"op":"createIndex","table":"notes","columns":[{"kind":"column","name":"tag"}],"name":"notes_tag_idx"}
    ]}"#;
    all_ops.extend(apply_doc(&be, mk_idx, &full, &live_tables, Approval::None).await);

    let drop_idx = r#"{"ir_version":1,"name":"drop_idx","ops":[
        {"op":"dropIndex","name":"notes_tag_idx","table":"notes"}
    ]}"#;
    all_ops.extend(apply_doc(&be, drop_idx, &full, &live_tables, Approval::None).await);

    // INTROSPECT the live SQLite schema.
    let live = be
        .snapshot_schema_sqlite()
        .await
        .expect("introspect live SQLite schema");

    // FOLD the SAME ops offline (no DB), SQLite dialect.
    let folded = fold_ops(&all_ops, SqlDialect::Sqlite, PROJECT).expect("fold the corpus offline");

    assert_eq!(
        canonicalize(folded),
        canonicalize(live),
        "fold_ops(corpus) must equal the live introspected SQLite snapshot"
    );
}
