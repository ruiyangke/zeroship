#![cfg(feature = "zsv8")]
//! PR4 test #1 (SQLite leg) + #6 — the end-to-end `new → edit → build → apply` flow
//! on a REAL temp-file SQLite backend, and the deterministic-by-construction scaffold.
//!
//! - Scaffold via `scaffold_new_ts`, edit the `.ts` to a real createTable+addColumn,
//!   `build_migrations` to produce transient canonical IR, then APPLY that IR
//!   through `IrAuthor::load_and_lower` + `engine.apply` on a real SQLite temp-file
//!   backend (LOCAL record path).
//! - The scaffold is deterministic by construction: it contains `now()` /
//!   `(col) => now()` + `genRandomUuid`, no `Date.now()`/`Math.random()`/
//!   `crypto.randomUUID()`, and recording it has ZERO determinism findings.
//!
//! Faithful: the REAL sandboxed recorder child + the REAL engine apply on SQLite.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use tempfile::TempDir;
use zeroship_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, SqlDialect,
    SqliteBackend,
};
use zeroship_migrate::frontend::recorder_service::recorder_child_path;
use zeroship_migrate::frontend::{
    build_migrations, scaffold_new_ts, RecordVia, ResourceBudget,
};

const PROJECT: &str = "prj_pr4";
const APP: &str = "app_pr4";

// An applyable migration: a createTable + addColumn with portable column types and
// a LITERAL default (synth defaults in createTable are a deferred render wave on the
// engine — the determinism scaffold demonstrates the synth pattern in comments).
const EDITED_TS: &str = r#"
import { table, t, now, genRandomUuid } from "@zeroship/migrate";
export function up() {
  table("notes").create({
    columns: {
      title: t.text().notNull(),
      status: t.text().notNull().default("active"),
    },
  });
  table("notes").column("body").add({ type: t.text() });
}
"#;

fn assert_child_built() {
    let bin = recorder_child_path();
    assert!(
        bin.exists(),
        "recorder child binary missing at {} — run `cargo build -p zeroship-migrate --bins` first",
        bin.display()
    );
}

fn via() -> RecordVia<'static> {
    RecordVia::Local {
        budget: ResourceBudget::default(),
    }
}

struct Paths {
    _dir: TempDir,
    app: std::path::PathBuf,
    journal: std::path::PathBuf,
}
fn backend_paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths { _dir: dir, app, journal }
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body.as_bytes()).unwrap();
}

#[test]
fn scaffold_is_deterministic_by_construction_and_records_zero_warnings() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();

    // `new` scaffolds the `.ts` (NO `.ir.json` at new time).
    let ts = scaffold_new_ts("seed_table").expect("scaffold a valid name");
    // Determinism-correct synth defaults documented.
    assert!(ts.contains("genRandomUuid()"));
    assert!(ts.contains("now()"));
    // Scan ONLY the executable body (comments stripped) for host clock / RNG — so
    // the guarantee is about the EMITTED ops, not comment text (LOW-fix).
    let code: String = ts
        .lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!code.contains("Date.now()"), "body:\n{code}");
    assert!(!code.contains("Math.random()"), "body:\n{code}");
    assert!(!code.contains("crypto.randomUUID()"), "body:\n{code}");
    assert!(!code.contains("new Date("), "body:\n{code}");

    // Recording the SCAFFOLD has ZERO determinism findings (the scaffold is
    // deterministic by construction).
    write(dir.path(), "20240617140000_seed_table.ts", &ts);
    let outcome = build_migrations(dir.path(), APP, &via()).expect("build the scaffold");
    let m = &outcome.migrations[0];
    assert!(
        m.warnings.is_empty(),
        "the scaffold must record ZERO determinism findings; got {:?}",
        m.warnings
    );
}

#[compio::test]
async fn new_edit_build_apply_on_sqlite() {
    assert_child_built();
    let mig_dir = tempfile::tempdir().unwrap();
    let stem = "20240617140000_notes";

    // `new` then EDIT the `.ts` to a real createTable + addColumn (the EDITED_TS).
    let _scaffold = scaffold_new_ts("notes").expect("scaffold");
    write(mig_dir.path(), &format!("{stem}.ts"), EDITED_TS);

    // `build` records via the LOCAL sandboxed child → transient canonical IR.
    let outcome = build_migrations(mig_dir.path(), APP, &via()).expect("build records transient IR");
    assert_eq!(outcome.migrations.len(), 1);
    let committed = String::from_utf8(outcome.migrations[0].committed_bytes.clone()).unwrap();
    assert!(committed.contains("createTable") || committed.contains("\"op\": \"createTable\""));
    assert!(
        !mig_dir.path().join(format!("{stem}.ir.json")).exists(),
        "build must not write a committed .ir.json"
    );

    // APPLY the transient `.ir.json` through the REAL gate + lower on SQLite.
    let p = backend_paths("notes");
    let be = SqliteBackend::open(&p.app, &p.journal).expect("open sqlite backend");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let migrations = author
        .load_and_lower(&committed, APP, &BTreeMap::new(), &LiveSchema::default(), None)
        .expect("the transient .ir.json must lower on SQLite");
    assert!(!migrations.is_empty());

    let engine = MigrationEngine::new();
    let guard = GuardConfig::confined_sqlite(PROJECT.to_string());
    let plan = engine.plan(&migrations, &guard);
    assert!(plan.denied.is_empty(), "no denials: {:?}", plan.denied);
    let applied = engine
        .apply(&plan, Approval::None, &be, &ExecutorConfig::new(PROJECT, PROJECT), "deploy")
        .await
        .expect("apply the committed IR on SQLite");
    assert!(!applied.applied.is_empty());

    // The table really exists with the createTable + addColumn columns.
    let rows = be
        .actor()
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name='notes'")
        .await
        .expect("sqlite_master probe");
    assert_eq!(rows.len(), 1, "the IR-created 'notes' table must exist on SQLite");
    let cols = be
        .actor()
        .query("SELECT name FROM pragma_table_info('notes') WHERE name IN ('title','body')")
        .await
        .expect("pragma_table_info");
    assert_eq!(cols.len(), 2, "the createTable 'title' + addColumn 'body' columns must exist");
}

#[compio::test]
async fn build_records_deterministic_transient_ir_without_writing_artifact() {
    assert_child_built();
    let mig_dir = tempfile::tempdir().unwrap();
    let stem = "20240617140000_notes";
    write(mig_dir.path(), &format!("{stem}.ts"), EDITED_TS);

    // First build records transient canonical IR.
    let first = build_migrations(mig_dir.path(), APP, &via()).expect("first build");
    assert_eq!(first.migrations[0].record_path, zeroship_migrate::frontend::RecordPath::Local);
    let first_bytes = first.migrations[0].committed_bytes.clone();

    // A second build re-records from the same deterministic source and the bytes are
    // identical; no committed artifact is used.
    let second = build_migrations(mig_dir.path(), APP, &via()).expect("second build");
    assert_eq!(
        second.migrations[0].record_path,
        zeroship_migrate::frontend::RecordPath::Local,
        "build records fresh transient IR"
    );
    assert_eq!(second.migrations[0].committed_bytes, first_bytes);
    assert!(
        !mig_dir.path().join(format!("{stem}.ir.json")).exists(),
        "build must not write a committed .ir.json"
    );
}
