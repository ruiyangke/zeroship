//! Faithful e2e for the creator IR envelope path on the `SQLite` leg,
//! driven through the REAL fail-closed LOAD GATE + lower (`IrAuthor::load_and_lower`)
//! and APPLIED on a real temp-file `SQLite` backend via the engine.
//!
//! This is the `SQLite` peer of the PG deploy e2e:
//! a valid IR envelope lowers + applies (the table exists, the migration journals),
//! and the SQLite-specific hostile case — an out-of-envelope `.splitPart`
//! against a `SQLite` target — is refused by the gate (`EXPR_NOT_PORTABLE`) before
//! any apply. No shims, no PG-gating: the real `SQLite` runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::{
    apply::executor::LockMode, resolve_create_table_policy, Approval, ExecutorConfig, GuardConfig,
    IrAuthor, LiveSchema, LoadAndLowerError, MigrationEngine, MigrationIr, PolicyProfile,
    SqlDialect, SqliteBackend,
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

fn resolved_envelope_json(raw: &str) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
    let resolved =
        resolve_create_table_policy(&ir, &PolicyProfile::confined()).expect("test IR resolves");
    serde_json::to_string(&resolved).expect("resolved test IR serializes")
}

// Happy path: a valid IR envelope createTable is gated (SQLite dialect), lowered,
// and APPLIED on a real SQLite backend — the table exists + journals.
#[compio::test]
async fn ir_envelope_lowers_and_applies_on_sqlite() {
    let p = paths("ir_apply");
    let be = backend(&p);

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false},
            {"name":"body","type":"text"}
        ]}
    ]}"#,
    );

    // The REAL fail-closed gate + lower, SQLite dialect.
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let migrations = author
        .load_and_lower(&ir, APP, &registry(&[]), &LiveSchema::default(), None)
        .expect("a valid IR envelope must lower on SQLite");
    assert!(!migrations.is_empty(), "lowering must yield migration(s)");

    // Apply through the engine on the real SQLite backend (Confined SQLite guard).
    let engine = MigrationEngine::new();
    let guard_cfg = GuardConfig::confined_sqlite(PROJECT.to_string());
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(
        plan.denied.is_empty(),
        "no denials on a clean IR set: {:?}",
        plan.denied
    );
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
    assert_eq!(
        rows.len(),
        1,
        "the IR-created 'notes' table must exist on SQLite"
    );
}

// L7/M15: `date` is an honest portable column type. The SQLite leg stores it as
// TEXT affinity and must accept/apply it through the real IR load gate.
#[compio::test]
async fn ir_envelope_date_column_lowers_and_applies_on_sqlite() {
    let p = paths("ir_date_apply");
    let be = backend(&p);

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_events","ops":[
        {"op":"createTable","name":"events","columns":[
            {"name":"happened_on","type":"date","nullable":false}
        ]}
    ]}"#,
    );

    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let migrations = author
        .load_and_lower(&ir, APP, &registry(&[]), &LiveSchema::default(), None)
        .expect("date columns must validate and lower on SQLite");
    assert!(
        migrations
            .iter()
            .any(|m| m.up.contains("\"happened_on\" TEXT NOT NULL")),
        "SQLite date column must render with TEXT affinity: {migrations:#?}"
    );

    let engine = MigrationEngine::new();
    let guard_cfg = GuardConfig::confined_sqlite(PROJECT.to_string());
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(
        plan.denied.is_empty(),
        "no denials on a date-column IR set: {:?}",
        plan.denied
    );
    engine
        .apply(&plan, Approval::None, &be, &exec_cfg(), "deploy-ir")
        .await
        .expect("apply the date-column IR on SQLite");

    let rows = be
        .actor()
        .query("SELECT type FROM pragma_table_info('events') WHERE name = 'happened_on'")
        .await
        .expect("pragma_table_info probe");
    assert_eq!(rows.len(), 1, "the date column must exist");
    assert_eq!(rows[0][0].as_deref(), Some("TEXT"));
}

// A LEGITIMATE portable string-literal column DEFAULT whose
// value contains the substring `;\n` (and a bare `;`) must lower CLEANLY through
// the PRODUCTION guarded path (`load_and_lower_guarded`) and APPLY on a real
// SQLite backend — the renderer's interior `;\n` (from `DEFAULT 'a;\nb'`) must NOT
// split the single CREATE statement. Pre-fix the textual fragment split broke the
// CREATE on the literal's `;\n`, tripping a guard denial / ReassemblyMismatch on
// a valid default. Post-fix the structural per-statement fragments keep the
// literal whole; the table materialises and a default-driven INSERT round-trips
// the embedded `;\n`. Driven through `apply_plan` (the shared orchestrator) over
// the guarded artifact's plan steps — the real deploy shape.
#[compio::test]
async fn ir_envelope_string_default_with_embedded_semicolon_newline_applies_on_sqlite() {
    let p = paths("ir_semicolon_default");
    let be = backend(&p);

    // The JSON `\n` escape yields the literal three-byte run `a ; \n b ; c`.
    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_docs","ops":[
        {"op":"createTable","name":"docs","columns":[
            {"name":"note","type":"text","nullable":false,
             "default":{"literal":{"value":"a;\nb;c"}}}
        ]}
    ]}"#,
    );

    // The REAL fail-closed gate + GUARDED lower (the production deploy entry).
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let guard_cfg = GuardConfig::confined_sqlite(PROJECT.to_string());
    let artifact = author
        .load_and_lower_guarded(
            &ir,
            APP,
            &registry(&[]),
            &LiveSchema::default(),
            &guard_cfg,
            None,
        )
        .expect("a portable ;\\n string default must lower through the guarded path on SQLite");

    // Per-statement attribution survived: the createTable's CREATE is ONE fragment
    // carrying the whole default (the interior `;\n` did NOT split it).
    let create_frag = artifact
        .fragments
        .iter()
        .find(|f| f.op_index == 0 && f.sql.contains("CREATE TABLE"))
        .expect("a CREATE TABLE fragment for op #0");
    assert_eq!(create_frag.op_kind, "createTable");
    assert!(
        create_frag.sql.contains("DEFAULT 'a;\nb;c'"),
        "the whole string default (incl. its ;\\n) stays inside one fragment; got {:?}",
        create_frag.sql
    );

    // Apply the guarded artifact's plan on the real SQLite backend.
    let engine = MigrationEngine::new();
    let outcome = engine
        .apply_plan(
            &artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "deploy-ir",
            LockMode::Acquire,
        )
        .await
        .expect("apply the guarded ;\\n-default IR on SQLite");
    assert!(
        !outcome.applied.applied.is_empty(),
        "the IR migration must apply"
    );

    // The table exists and the stored CREATE SQL preserved the embedded `;\n`.
    let create_sql = be
        .actor()
        .query("SELECT sql FROM sqlite_master WHERE type='table' AND name='docs'")
        .await
        .expect("sqlite_master probe");
    assert_eq!(
        create_sql.len(),
        1,
        "the IR-created 'docs' table must exist on SQLite"
    );

    // The default really drives an INSERT: a row that omits `note` gets `a;\nb;c`.
    be.actor()
        .exec(
            "INSERT INTO docs (id, created_at, updated_at, version) \
             VALUES ('doc_1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)",
        )
        .await
        .expect("insert a row relying on the column default");
    let rows = be
        .actor()
        .query("SELECT note FROM docs WHERE id = 'doc_1'")
        .await
        .expect("read back the defaulted row");
    assert_eq!(rows.len(), 1, "one row inserted");
    let note = rows[0][0].as_deref().expect("note is non-null").to_string();
    assert_eq!(
        note, "a;\nb;c",
        "the column default with its embedded ;\\n must apply verbatim"
    );
}

// HOSTILE (SQLite-specific) — an out-of-envelope `.splitPart` in a backfill
// SET against a SQLite target is refused by the gate (EXPR_NOT_PORTABLE) BEFORE
// any apply. `splitPart` is PG-expressible but out-of-envelope on SQLite, so
// the dialect-parameterized validate refuses it fail-closed.
#[compio::test]
async fn out_of_envelope_splitpart_refused_on_sqlite() {
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
        .load_and_lower(ir, APP, &registry(&[("users", APP)]), &(&live).into(), None)
        .expect_err("an out-of-envelope splitPart must be refused on SQLite");
    match err {
        LoadAndLowerError::Load(zero_migrate::IrLoadError::Validate(ae)) => {
            assert_eq!(
                ae.code,
                zero_migrate::model::validate::CODE_EXPR_NOT_PORTABLE,
                "the SQLite-out-of-envelope splitPart is EXPR_NOT_PORTABLE; got {:?}",
                ae.code
            );
        }
        other => panic!("expected a fail-closed Load(Validate EXPR_NOT_PORTABLE), got: {other}"),
    }
}
