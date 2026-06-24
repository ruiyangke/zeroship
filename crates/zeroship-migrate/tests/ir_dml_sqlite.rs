//! §PR6a — FAITHFUL e2e for the creator DML surface on the **real SQLite** leg
//! (temp-file backend, no shim, no PG-gating). Drives the REAL load-gate + lower
//! (`IrAuthor::load_and_lower`) → `MigrationEngine::apply_plan` → a real hardened
//! SQLite connection, then probes the rows to prove the data transform actually
//! happened.
//!
//! Coverage (the spec's PR6a SQLite obligations):
//! - **one-shot DML portable on SQLite**: seed via `insert`, transform via one-shot
//!   `update`, prune via `del` — each applies on a real SQLite file and the row
//!   state proves it.
//! - **bind-safety**: an `insert`/`update` whose value carries SQL metacharacters
//!   (quote/semicolon/comment) is stored VERBATIM (one native `?n` bind) and cannot
//!   alter the statement shape — the table and its rows survive intact.
//! - **`onConflict` is a HARD error on SQLite** (`DmlError::OnConflictNotPortable`,
//!   `dialect_scope = PgOnly`) — rejected at lower, never silently applied.
//! - **a batched `backfill` is a HARD error on SQLite** (PR6b boundary) — rejected
//!   at lower, never silently downgraded.
//! - **column-scoping (rule (c), §3.3.1.1(c))**: a `ColRef` to a column not on the
//!   LIVE target table is rejected with the structured `UNSUPPORTED { kind: "expr" }`
//!   AuthoringError at the resolved apply/render seam (`validate_op_resolved`,
//!   driven from the introspected `LiveSchema::table_snapshots`) BEFORE the template
//!   is assembled — never baked in to surface as a raw "no such column" DB error.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::{
    executor::LockMode, Approval, ExecutorConfig, IrAuthor, IrLowerError, LiveSchema,
    MigrationEngine, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_dml";
const APP: &str = "app_dml";

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

/// Lower a `.ir.json` on SQLite + apply its plan on the real backend, asserting
/// success. Returns nothing (the caller probes the backend).
async fn lower_and_apply(
    be: &SqliteBackend,
    ir: &str,
    reg: &BTreeMap<String, String>,
    approval: Approval,
) {
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let migrations = author
        .load_and_lower(ir, APP, reg, &LiveSchema::default())
        .expect("lower the .ir.json on SQLite");
    let engine = MigrationEngine::new();
    let plan = zeroship_migrate::plan::AppliedPlan {
        version: zeroship_migrate::migration::MigrationId::generate(),
        name: "dml".into(),
        steps: migrations.into_iter().map(zeroship_migrate::plan::PlanStep::Ddl).collect(),
        checksum: zeroship_migrate::migration::Checksum::of(&zeroship_migrate::migration::ChecksumInput {
            up: "x",
            down: None,
            flags: &zeroship_migrate::migration::MigrationFlags::default(),
            owner_app: APP,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: zeroship_migrate::migration::MigrationFlags::default(),
        dialect_scope: zeroship_migrate::plan::DialectScope::Both,
        rollbackable: false,
        owner_app: APP.into(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
    };
    engine
        .apply_plan(&plan.steps, approval, be, &exec_cfg(), "deploy", LockMode::Acquire)
        .await
        .expect("apply the DDL plan on SQLite");
}

/// Lower the full DML plan (DDL + DML steps) through `lower_plan` and apply it —
/// the real authoring path. Returns the applied outcome.
async fn lower_plan_and_apply(
    be: &SqliteBackend,
    ir: &str,
    reg: &BTreeMap<String, String>,
    approval: Approval,
) {
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::ir_load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::validate::Dialect::Sqlite,
        reg,
    )
    .expect("load gate");
    let plan = author
        .lower_plan(&document, &LiveSchema::default())
        .expect("lower the full DML plan on SQLite");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(&plan.steps, approval, be, &exec_cfg(), "deploy", LockMode::Acquire)
        .await
        .expect("apply the DML plan on SQLite");
}

fn create_codes_table() -> &'static str {
    r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false},
            {"name":"label","type":"text"}
        ]}
    ]}"#
}

// Seed the four NOT-NULL system fields + the user columns for an insert.
fn seed_ir() -> &'static str {
    r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[
            ["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,200,"ok"],
            ["c2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,404,null]
         ]}
    ]}"#
}

#[compio::test]
async fn one_shot_insert_update_delete_apply_on_sqlite() {
    let p = paths("one_shot");
    let be = backend(&p);

    // DDL: create the table (own apply, DDL plan).
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;

    // INSERT two rows (the authoring path → Dml step → apply_plan → real SQLite).
    lower_plan_and_apply(&be, seed_ir(), &registry(&[("codes", APP)]), Approval::None).await;
    let rows = be
        .actor()
        .query("SELECT code, label FROM codes ORDER BY code")
        .await
        .expect("probe");
    assert_eq!(rows.len(), 2, "two seed rows must exist");
    assert_eq!(rows[0], vec![Some("200".into()), Some("ok".into())]);
    assert_eq!(rows[1], vec![Some("404".into()), None]);

    // UPDATE: coalesce the null label to 'unknown' where code > 300.
    let update_ir = r#"{"ir_version":1,"name":"fixup","ops":[
        {"op":"update","table":"codes",
         "set":{"label":{"node":"fnCall","fn":"coalesce","args":[
             {"node":"colRef","name":"label"},
             {"node":"literal","value":"unknown"}]}},
         "where":{"node":"binOp","op":"gt",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":300}}}
    ]}"#;
    lower_plan_and_apply(&be, update_ir, &registry(&[("codes", APP)]), Approval::None).await;
    let rows = be
        .actor()
        .query("SELECT label FROM codes WHERE code = 404")
        .await
        .expect("probe");
    assert_eq!(rows[0], vec![Some("unknown".into())], "the one-shot update ran on SQLite");

    // DELETE: prune the row whose code = 200 (destructive ⇒ needs approval).
    let delete_ir = r#"{"ir_version":1,"name":"prune","ops":[
        {"op":"delete","table":"codes",
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":200}}}
    ]}"#;
    lower_plan_and_apply(&be, delete_ir, &registry(&[("codes", APP)]), Approval::Approved).await;
    let rows = be.actor().query("SELECT code FROM codes ORDER BY code").await.expect("probe");
    assert_eq!(rows.len(), 1, "one row pruned");
    assert_eq!(rows[0], vec![Some("404".into())]);
}

/// Bind-safety on SQLite: a value full of SQL metacharacters is stored VERBATIM as
/// one native `?n` bind — it cannot alter the statement shape. The table survives
/// (a successful "DROP TABLE" injection would make the next probe fail) and the
/// stored value is byte-identical to the hostile input.
#[compio::test]
async fn bind_safety_metacharacters_on_sqlite() {
    let p = paths("bind_safety");
    let be = backend(&p);
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;

    let hostile = "x'); DROP TABLE codes; --";
    let insert_ir = format!(
        r#"{{"ir_version":1,"name":"seed","ops":[
            {{"op":"insert","table":"codes",
             "columns":["id","created_at","updated_at","version","code","label"],
             "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,1,{hostile:?}]]}}
        ]}}"#
    );
    lower_plan_and_apply(&be, &insert_ir, &registry(&[("codes", APP)]), Approval::None).await;

    // The table still exists (the injection did NOT execute) and the hostile string
    // is stored verbatim as data.
    let rows = be.actor().query("SELECT label FROM codes WHERE code = 1").await.expect("probe");
    assert_eq!(
        rows[0],
        vec![Some(hostile.to_string())],
        "the metacharacter value must be stored verbatim, not interpreted"
    );
}

/// `insert { onConflict }` is a HARD authoring error on SQLite (PG-only, §9). The
/// IDENTICAL `.ir.json` would load+render on PG (covered in the PG e2e); here it is
/// rejected at LOWER with `DmlError::OnConflictNotPortable` — never silently
/// applied without the conflict clause.
#[compio::test]
async fn on_conflict_rejected_on_sqlite() {
    let ir = r#"{"ir_version":1,"name":"upsert","ops":[
        {"op":"insert","table":"codes","columns":["code"],"rows":[[1]],
         "onConflict":{"columns":["code"]}}
    ]}"#;
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::ir_load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
    )
    .expect("load gate (onConflict is a LOWER-time reject, not a load-gate reject)");
    let err = author
        .lower_plan(&document, &LiveSchema::default())
        .expect_err("onConflict must be rejected on SQLite");
    assert!(
        matches!(err, IrLowerError::DmlAssemble(zeroship_migrate::dml::DmlError::OnConflictNotPortable { .. })),
        "expected OnConflictNotPortable, got {err:?}"
    );
}

/// A batched `backfill` is a HARD error on SQLite (the SQLite batched executor is
/// PR6b). Rejected at lower — never silently downgraded to a one-shot UPDATE.
#[compio::test]
async fn batched_backfill_rejected_on_sqlite() {
    let ir = r#"{"ir_version":1,"name":"bf","ops":[
        {"op":"backfill","table":"codes","cursorColumn":"code","batchSize":100,
         "set":{"label":{"node":"literal","value":"x"}},"name":"fill_labels"}
    ]}"#;
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::ir_load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
    )
    .expect("load gate");
    let err = author
        .lower_plan(&document, &LiveSchema::default())
        .expect_err("a SQLite-targeted batched backfill must be rejected (PR6b)");
    assert!(
        matches!(err, IrLowerError::DmlAssemble(zeroship_migrate::dml::DmlError::BatchedBackfillNotPortable { .. })),
        "expected BatchedBackfillNotPortable, got {err:?}"
    );
}

/// Structural-validator portable expressions apply on SQLite: a `cast("integer")`
/// and a `concat` with a NULL operand are portable closed-AST nodes (§3.3.1) — they
/// lower + apply cleanly on the real SQLite backend (the PG leg is covered by the
/// assembler unit tests + the PG e2e). Concat with NULL propagates NULL on BOTH
/// backends (the one place PG/SQLite `||` semantics agree), and the cast renders
/// portably.
#[compio::test]
async fn portable_cast_and_concat_apply_on_sqlite() {
    let p = paths("portable_expr");
    let be = backend(&p);
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;
    lower_plan_and_apply(&be, seed_ir(), &registry(&[("codes", APP)]), Approval::None).await;

    // UPDATE: label = cast(code as text) || NULL  → NULL (concat propagates NULL),
    // exercised on the row code=200. The cast + concat are portable nodes.
    let ir = r#"{"ir_version":1,"name":"expr","ops":[
        {"op":"update","table":"codes",
         "set":{"label":{"node":"binOp","op":"concat",
             "lhs":{"node":"cast","operand":{"node":"colRef","name":"code"},"target":"text"},
             "rhs":{"node":"literal","value":null}}},
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":200}}}
    ]}"#;
    lower_plan_and_apply(&be, ir, &registry(&[("codes", APP)]), Approval::None).await;
    let rows = be.actor().query("SELECT label FROM codes WHERE code = 200").await.expect("probe");
    assert_eq!(rows[0], vec![None], "concat with a NULL operand propagates NULL on SQLite");
}

/// **PR6a `c.fn.concatWs` SQLite lowering — faithful apply coverage (§9).** SQLite
/// has no native `concat_ws`, so the assembler lowers `concatWs(delim, a, b, …)` to
/// a pinned NULL-skipping `||`-fold + head-trim. PG's `concat_ws` SKIPS NULL
/// arguments; the SQLite fold must produce a BYTE-IDENTICAL result. This drives the
/// SQLite NULL-skip lowering end-to-end on a real backend over a NULL operand: an
/// `update` sets `label = concatWs('-', cast(code as text), label)` on a row whose
/// `label` IS NULL — PG would yield `'1'` (the NULL `label` is skipped, no trailing
/// delimiter), so the SQLite head-trim lowering must yield exactly `'1'` too. (The
/// PG render leg is covered by the assembler unit tests + the PG e2e; this pins the
/// SQLite head-trim before any creator can reach it.)
#[compio::test]
async fn concat_ws_null_skip_applies_byte_identical_on_sqlite() {
    let p = paths("concat_ws");
    let be = backend(&p);
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;

    // Seed a single row: code=1, label=NULL.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,1,null]]}
    ]}"#;
    lower_plan_and_apply(&be, seed, &registry(&[("codes", APP)]), Approval::None).await;

    // UPDATE: label = concatWs('-', cast(code as text), label)  where code = 1.
    // label IS NULL ⇒ concat_ws skips it ⇒ result is just '1' (no trailing '-').
    let ir = r#"{"ir_version":1,"name":"cws","ops":[
        {"op":"update","table":"codes",
         "set":{"label":{"node":"fnSynth","fn":"concatWs","args":[
             {"node":"literal","value":"-"},
             {"node":"cast","operand":{"node":"colRef","name":"code"},"target":"text"},
             {"node":"colRef","name":"label"}]}},
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":1}}}
    ]}"#;
    lower_plan_and_apply(&be, ir, &registry(&[("codes", APP)]), Approval::None).await;
    let rows = be.actor().query("SELECT label FROM codes WHERE code = 1").await.expect("probe");
    assert_eq!(
        rows[0],
        vec![Some("1".to_string())],
        "concatWs NULL-skip on SQLite must match PG concat_ws('-', '1', NULL) = '1' (no trailing delimiter)"
    );
}

/// **PR6a rule-(c) column-scoping at the apply/render seam (§3.3.1.1(c)) on the
/// real SQLite leg.** An `update` whose SET RHS references a column NOT on the live
/// target table must be rejected with the STRUCTURED `UNSUPPORTED { kind: "expr" }`
/// AuthoringError at lower/apply — BEFORE the template is assembled — NOT surface as
/// a raw SQLite "no such column" error at execution. The live `LiveSchema` is built
/// from REAL temp-file introspection (`snapshot_schema_sqlite`), then the authoring
/// path (`lower_plan`) is driven with it. A NOTHING-applied reject (the SQLite peer
/// of the PG `ir_unresolved_colref_rejected_at_apply_seam_on_pg`).
#[compio::test]
async fn unresolved_colref_rejected_at_apply_seam_on_sqlite() {
    let p = paths("colref_scope");
    let be = backend(&p);

    // DDL: create `codes` (code, label).
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;

    // Introspect the LIVE schema so the lower sees the real live columns of `codes`
    // (system fields + code/label).
    let live = be.snapshot_schema_sqlite().await.expect("introspect live SQLite schema");
    let live_schema = LiveSchema {
        tables: live.tables.keys().cloned().collect(),
        table_snapshots: live.tables.clone(),
        ..LiveSchema::default()
    };
    let codes_cols: Vec<String> = live_schema.table_snapshots["codes"]
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    // The resolution must be ACTIVE (not skipped on an empty snapshot): the live
    // `codes` table really carries `code`/`label` (and the system fields), and NOT
    // `ghost` — so the reject below is a true rule-(c) failure, not a vacuous pass.
    assert!(
        codes_cols.iter().any(|c| c == "code") && codes_cols.iter().any(|c| c == "label"),
        "live `codes` snapshot must surface its real columns (got {codes_cols:?})"
    );
    assert!(
        !codes_cols.iter().any(|c| c == "ghost"),
        "the live `codes` table must NOT carry `ghost` (got {codes_cols:?})"
    );

    // An update whose SET RHS names `ghost` — a column NOT on the live `codes`
    // table. Loads clean (load is structural-only for DML), but the resolved
    // apply/render seam must reject it.
    let bad = r#"{"ir_version":1,"name":"bad","ops":[
        {"op":"update","table":"codes",
         "set":{"label":{"node":"colRef","name":"ghost"}},
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":1}}}
    ]}"#;
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::ir_load::load_ir_document(
        bad,
        APP,
        zeroship_migrate::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
    )
    .expect("load gate (DML ColRef resolution is an apply-seam reject, not a load reject)");
    let err = author
        .lower_plan(&document, &live_schema)
        .expect_err("an unresolved ColRef must be rejected at the resolved apply seam, pre-assembly");
    match err {
        IrLowerError::DmlValidate(e) => {
            assert_eq!(e.code, zeroship_migrate::CODE_UNSUPPORTED, "structured (c) reject");
            assert_eq!(
                e.kind,
                Some(zeroship_migrate::UnsupportedKind::Expr),
                "rule (c) is an expr-kind capability-boundary reject"
            );
        }
        other => panic!("expected DmlValidate UNSUPPORTED expr, got {other:?}"),
    }
}

/// Identifier safety at assembly: a malformed `set`-column identifier (carrying SQL
/// metacharacters) is rejected by the assembler's identifier gate BEFORE any SQL is
/// emitted — the injection cannot reach the database through an identifier slot.
/// (The rule-(c) `ColRef`-resolution against the live target table is exercised
/// end-to-end on this real SQLite backend by
/// `unresolved_colref_rejected_at_apply_seam_on_sqlite` above, and on PG by
/// `ir_unresolved_colref_rejected_at_apply_seam_on_pg`.)
#[compio::test]
async fn malformed_set_identifier_rejected_on_sqlite() {
    let bad_ident_ir = r#"{"ir_version":1,"name":"bad","ops":[
        {"op":"update","table":"codes",
         "set":{"label\"; DROP TABLE codes; --":{"node":"literal","value":"x"}}}
    ]}"#;
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::ir_load::load_ir_document(
        bad_ident_ir,
        APP,
        zeroship_migrate::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
    )
    .expect("load gate");
    let err = author
        .lower_plan(&document, &LiveSchema::default())
        .expect_err("a malformed set-column identifier must be rejected at assembly");
    assert!(
        matches!(err, IrLowerError::DmlAssemble(zeroship_migrate::dml::DmlError::InvalidIdentifier { .. })),
        "expected InvalidIdentifier, got {err:?}"
    );
}
