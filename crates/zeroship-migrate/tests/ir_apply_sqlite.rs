//! §6 / §8.6 — faithful e2e for the creator `.ir.json` path on the SQLite leg,
//! driven through the REAL fail-closed LOAD GATE + lower (`IrAuthor::load_and_lower`)
//! and APPLIED on a real temp-file SQLite backend via the engine.
//!
//! This is the SQLite peer of the control-plane `deploy_migrate_test.rs` PG e2e:
//! a valid `.ir.json` lowers + applies (the table exists, the migration journals),
//! and the SQLite-specific hostile case — an out-of-envelope `c.fn.splitPart`
//! against a SQLite target — is refused by the gate (`EXPR_NOT_PORTABLE`) before
//! any apply. No shims, no PG-gating: the real SQLite runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LoadAndLowerError,
    MigrationEngine, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_ir";
const APP: &str = "app_ir";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths { _dir: dir, app, journal }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT)
}

fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
}

// Happy path: a valid `.ir.json` createTable is gated (SQLite dialect), lowered,
// and APPLIED on a real SQLite backend — the table exists + journals.
#[compio::test]
async fn ir_json_lowers_and_applies_on_sqlite() {
    let p = paths("ir_apply");
    let be = backend(&p);

    let ir = r#"{"ir_version":1,"name":"create_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false},
            {"name":"body","type":"text"}
        ]}
    ]}"#;

    // The REAL fail-closed gate + lower, SQLite dialect.
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let migrations = author
        .load_and_lower(ir, APP, &registry(&[]), &LiveSchema::default())
        .expect("a valid .ir.json must lower on SQLite");
    assert!(!migrations.is_empty(), "lowering must yield migration(s)");

    // Apply through the engine on the real SQLite backend (Confined SQLite guard).
    let engine = MigrationEngine::new();
    let guard_cfg = GuardConfig::confined_sqlite(PROJECT.to_string());
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(plan.denied.is_empty(), "no denials on a clean IR set: {:?}", plan.denied);
    let outcome = engine
        .apply(&plan, Approval::None, &be, &exec_cfg(), "deploy-ir")
        .await
        .expect("apply the lowered IR on SQLite");
    assert!(!outcome.applied.is_empty(), "the IR migration must apply");

    // The table really exists in the SQLite app file.
    let rows = be
        .actor()
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name='notes'")
        .await
        .expect("sqlite_master probe");
    assert_eq!(rows.len(), 1, "the IR-created 'notes' table must exist on SQLite");
}

// HOSTILE (SQLite-specific) — an out-of-envelope `c.fn.splitPart` in a backfill
// SET against a SQLite target is refused by the gate (EXPR_NOT_PORTABLE) BEFORE
// any apply. `splitPart` is PG-expressible but out-of-envelope on SQLite (§9), so
// the dialect-parameterized validate refuses it fail-closed.
#[compio::test]
async fn ir_json_out_of_envelope_splitpart_refused_on_sqlite() {
    // An update whose SET applies a MULTI-CHAR-delim splitPart — in-envelope on PG,
    // out-of-envelope on SQLite. The gate is dialect-parameterized, so the SAME
    // artifact loads on PG and is REFUSED on SQLite (the gate runs BEFORE lower, so
    // the DML-not-yet-lowerable arm is never reached here).
    let ir = r#"{"ir_version":1,"name":"split_update","ops":[
        {"op":"update","table":"users","set":{
            "name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"full_name"},
                {"node":"literal","value":", "},
                {"node":"literal","value":1}
            ]}
        }}
    ]}"#;

    let mut live = BTreeSet::new();
    live.insert("users".to_string());
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let err = author
        .load_and_lower(ir, APP, &registry(&[("users", APP)]), &(&live).into())
        .expect_err("an out-of-envelope splitPart must be refused on SQLite");
    match err {
        LoadAndLowerError::Load(zeroship_migrate::IrLoadError::Validate(ae)) => {
            assert_eq!(
                ae.code,
                zeroship_migrate::validate::CODE_EXPR_NOT_PORTABLE,
                "the SQLite-out-of-envelope splitPart is EXPR_NOT_PORTABLE; got {:?}",
                ae.code
            );
        }
        other => panic!("expected a fail-closed Load(Validate EXPR_NOT_PORTABLE), got: {other}"),
    }
}
