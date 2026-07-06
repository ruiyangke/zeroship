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
//! - **`onConflict` is a HARD error on SQLite** (`dialect_scope = PgOnly`) —
//!   rejected at validate-time, never silently applied.
//! - **a batched `backfill` is PORTABLE on SQLite** (PR6b) — lowers to a
//!   `PlanStep::Backfill` the SQLite batched executor consumes (§2.3.1).
//! - **column-scoping (rule (c), §3.3.1.1(c))**: a `ColRef` to a column not on the
//!   LIVE target table is rejected with the structured `UNSUPPORTED { kind: "expr" }`
//!   AuthoringError at the resolved apply/render seam (`validate_op_resolved`,
//!   driven from the introspected `LiveSchema::table_snapshots`) BEFORE the template
//!   is assembled — never baked in to surface as a raw "no such column" DB error.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::{
    apply::executor::LockMode, frontend::record_migration_to_ir_unsandboxed, Approval, ExecutorConfig, IrAuthor, IrLowerError, LiveSchema,
    MigrationEngine, MigrationIr, PolicyProfile, SqlDialect, SqliteBackend,
    resolve_create_table_policy,
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

fn resolve_ir(ir: &MigrationIr) -> MigrationIr {
    resolve_create_table_policy(ir, &PolicyProfile::confined()).expect("test IR resolves")
}

fn resolved_ir_json(raw: &str) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
    serde_json::to_string(&resolve_ir(&ir)).expect("resolved test IR serializes")
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_secs() as i64
}

fn parse_sqlite_current_timestamp(value: &str) -> i64 {
    assert_eq!(value.len(), 19, "SQLite CURRENT_TIMESTAMP shape changed: {value:?}");
    assert_eq!(&value[4..5], "-", "SQLite CURRENT_TIMESTAMP shape changed: {value:?}");
    assert_eq!(&value[7..8], "-", "SQLite CURRENT_TIMESTAMP shape changed: {value:?}");
    assert_eq!(&value[10..11], " ", "SQLite CURRENT_TIMESTAMP shape changed: {value:?}");
    assert_eq!(&value[13..14], ":", "SQLite CURRENT_TIMESTAMP shape changed: {value:?}");
    assert_eq!(&value[16..17], ":", "SQLite CURRENT_TIMESTAMP shape changed: {value:?}");

    let year: i64 = value[0..4].parse().expect("timestamp year parses");
    let month: i64 = value[5..7].parse().expect("timestamp month parses");
    let day: i64 = value[8..10].parse().expect("timestamp day parses");
    let hour: i64 = value[11..13].parse().expect("timestamp hour parses");
    let minute: i64 = value[14..16].parse().expect("timestamp minute parses");
    let second: i64 = value[17..19].parse().expect("timestamp second parses");

    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 { adjusted_year } else { adjusted_year - 399 } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_index = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;

    days_since_epoch * 86_400 + hour * 3_600 + minute * 60 + second
}

/// Lower a `.ir.json` on SQLite + apply its plan on the real backend, asserting
/// success. Returns nothing (the caller probes the backend).
async fn lower_and_apply(
    be: &SqliteBackend,
    ir: &str,
    reg: &BTreeMap<String, String>,
    approval: Approval,
) {
    let ir = resolved_ir_json(ir);
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let migrations = author
        .load_and_lower(&ir, APP, reg, &LiveSchema::default(), None)
        .expect("lower the .ir.json on SQLite");
    let engine = MigrationEngine::new();
    let plan = zeroship_migrate::AppliedPlan {
        version: zeroship_migrate::model::migration::MigrationId::generate(),
        name: "dml".into(),
        steps: migrations.into_iter().map(zeroship_migrate::PlanStep::Ddl).collect(),
        checksum: zeroship_migrate::model::migration::Checksum::of(&zeroship_migrate::model::migration::ChecksumInput {
            up: "x",
            down: None,
            flags: &zeroship_migrate::model::migration::MigrationFlags::default(),
            owner_app: APP,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: zeroship_migrate::model::migration::MigrationFlags::default(),
        dialect_scope: zeroship_migrate::DialectScope::Both,
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
) -> zeroship_migrate::engine::DeclarativeDeployOutcome {
    let ir = resolved_ir_json(ir);
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::model::load::load_ir_document(
        &ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Sqlite,
        reg,
        None,
        None,
    )
    .expect("load gate");
    let plan = author
        .lower_plan(&document, &LiveSchema::default())
        .expect("lower the full DML plan on SQLite");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(&plan.steps, approval, be, &exec_cfg(), "deploy", LockMode::Acquire)
        .await
        .expect("apply the DML plan on SQLite")
}

async fn load_recorded_ir_and_apply(
    be: &SqliteBackend,
    ir: &zeroship_migrate::MigrationIr,
    reg: &BTreeMap<String, String>,
) {
    let resolved = resolve_ir(ir);
    let bytes = serde_json::to_string(&resolved).expect("recorded IR serializes");
    let document = zeroship_migrate::model::load::load_ir_document(
        &bytes,
        APP,
        zeroship_migrate::model::validate::Dialect::Sqlite,
        reg,
        None,
        None,
    )
    .expect("load recorded IR through the SQLite gate");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let plan = author
        .lower_plan(&document, &LiveSchema::default())
        .expect("render recorded fnSynth IR on SQLite");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(&plan.steps, Approval::None, be, &exec_cfg(), "deploy", LockMode::Acquire)
        .await
        .expect("apply recorded fnSynth IR on SQLite");
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

#[compio::test]
async fn update_set_scalar_and_expr_apply_on_sqlite() {
    let p = paths("mixed_set");
    let be = backend(&p);

    let create = r#"{"ir_version":1,"name":"create_mix","ops":[
        {"op":"createTable","name":"mix","columns":[
            {"name":"base","type":"int","nullable":false},
            {"name":"scalar_val","type":"int","nullable":false},
            {"name":"expr_val","type":"int","nullable":false}
        ]}
    ]}"#;
    lower_and_apply(&be, create, &registry(&[]), Approval::None).await;

    let seed = r#"{"ir_version":1,"name":"seed_mix","ops":[
        {"op":"insert","table":"mix",
         "columns":["id","created_at","updated_at","version","base","scalar_val","expr_val"],
         "rows":[["m1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,5,0,0]]}
    ]}"#;
    lower_plan_and_apply(&be, seed, &registry(&[("mix", APP)]), Approval::None).await;

    let update = r#"{"ir_version":1,"name":"update_mix","ops":[
        {"op":"update","table":"mix",
         "set":{"scalar_val":7,"expr_val":{"node":"binOp","op":"add",
             "lhs":{"node":"colRef","name":"base"},
             "rhs":{"node":"literal","value":1}}},
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"id"},
             "rhs":{"node":"literal","value":"m1"}}}
    ]}"#;
    lower_plan_and_apply(&be, update, &registry(&[("mix", APP)]), Approval::None).await;

    let rows = be
        .actor()
        .query("SELECT scalar_val, expr_val FROM mix WHERE id = 'm1'")
        .await
        .expect("read mixed set update proof row");
    assert_eq!(
        rows,
        vec![vec![Some("7".into()), Some("6".into())]],
        "SQLite update.set scalar and expression values apply identically"
    );
}

#[compio::test]
async fn recorded_fnsynth_symbol_insert_applies_db_evaluated_values_on_sqlite() {
    let p = paths("fnsynth_symbol");
    let be = backend(&p);

    let create = r#"{"ir_version":1,"name":"create_events","ops":[
        {"op":"createTable","name":"events","columns":[
            {"name":"kind","type":"text"}
        ]}
    ]}"#;
    lower_and_apply(&be, create, &registry(&[]), Approval::None).await;

    let symbol_src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "seed_symbols", up() {
            table("events").insert({ rows: [{
                id: crypto.randomUUID,
                created_at: Date.now,
                updated_at: Date.now,
                version: 1,
                kind: "symbol"
            }] });
        }};
    "#;
    let explicit_src = r#"
        import { table, cFn } from "@zeroship/migrate";
        export default { name: "seed_symbols", up() {
            table("events").insert({ rows: [{
                id: cFn.genRandomUuid(),
                created_at: cFn.now(),
                updated_at: cFn.now(),
                version: 1,
                kind: "symbol"
            }] });
        }};
    "#;
    let recorded_at = unix_secs();
    let symbol_ir = record_migration_to_ir_unsandboxed(symbol_src, APP, "sqlite_fnsynth_symbol")
        .expect("record symbol-form fnSynth insert");
    let explicit_ir = record_migration_to_ir_unsandboxed(explicit_src, APP, "sqlite_fnsynth_explicit")
        .expect("record explicit cFn insert");
    assert_eq!(
        symbol_ir.ops, explicit_ir.ops,
        "native symbol form and cFn form must record byte-identical ops"
    );

    compio::time::sleep(std::time::Duration::from_secs(2)).await;
    let apply_start = unix_secs();
    load_recorded_ir_and_apply(&be, &symbol_ir, &registry(&[("events", APP)])).await;
    let apply_end = unix_secs();

    let rows = be
        .actor()
        .query("SELECT id, created_at FROM events WHERE kind = 'symbol'")
        .await
        .expect("read fnSynth-applied row");
    assert_eq!(rows.len(), 1, "one fnSynth row must be inserted");
    let id = rows[0][0].as_deref().expect("id is non-null");
    let created_epoch = parse_sqlite_current_timestamp(
        rows[0][1]
            .as_deref()
            .expect("created_at is non-null"),
    );

    uuid::Uuid::parse_str(id)
        .unwrap_or_else(|e| panic!("id must be a DB-generated uuid-ish value, got {id:?}: {e}"));
    assert!(
        created_epoch >= recorded_at + 1,
        "created_at must be evaluated at apply time, not record time: record={recorded_at}, stored={created_epoch}"
    );
    assert!(
        (apply_start - 1..=apply_end + 5).contains(&created_epoch),
        "created_at must land in the apply window: apply={apply_start}..{apply_end}, stored={created_epoch}"
    );
}

/// HIGH — `op.del(table, where, {limit})` must apply on SQLite. The bundled
/// rusqlite (features=["bundled"]) does NOT compile `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`,
/// so a literal `DELETE … WHERE … LIMIT ?1` is a HARD syntax error at apply. The
/// portable lowering is the `rowid IN (SELECT rowid … WHERE … LIMIT ?1)` subquery
/// form (the SQLite analog of the PG ctid form). This proves a `del` with a limit
/// LOWER than the number of matching rows deletes EXACTLY `limit` rows on a real
/// SQLite file — pre-fix it failed with `near "LIMIT": syntax error`.
#[compio::test]
async fn del_with_limit_deletes_exactly_n_on_sqlite() {
    let p = paths("del_limit");
    let be = backend(&p);
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;

    // Seed FIVE rows that all match the WHERE (code >= 0) so the limit truly bites.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[
            ["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,1,"a"],
            ["c2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,2,"b"],
            ["c3","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,3,"c"],
            ["c4","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,4,"d"],
            ["c5","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,5,"e"]
         ]}
    ]}"#;
    lower_plan_and_apply(&be, seed, &registry(&[("codes", APP)]), Approval::None).await;

    // DELETE WHERE code >= 1 LIMIT 2 — all five rows match, but only 2 may go.
    let delete_ir = r#"{"ir_version":1,"name":"prune_limited","ops":[
        {"op":"delete","table":"codes",
         "where":{"node":"binOp","op":"ge",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":1}},
         "limit":2}
    ]}"#;
    lower_plan_and_apply(&be, delete_ir, &registry(&[("codes", APP)]), Approval::Approved).await;

    let rows = be.actor().query("SELECT code FROM codes ORDER BY code").await.expect("probe");
    assert_eq!(rows.len(), 3, "del{{limit:2}} must remove EXACTLY 2 of 5 matching rows on SQLite");
}

/// MED — two BYTE-IDENTICAL DML ops in the SAME migration must each apply. The
/// per-step journal version is derived from the step content; if the op's plan
/// position is NOT folded in, two identical ops collide to ONE version and the
/// second is net-applied-skipped (silent data-intent loss). Two byte-identical
/// `id`-bearing inserts can't both land (the TEXT PK would conflict), so the
/// observable proof uses two BYTE-IDENTICAL increment UPDATEs (same template +
/// same binds): each bumps `code` by 1 on the single seed row. Pre-fix the second
/// op collides to the first's version and is silently skipped (code = 1); post-fix
/// the distinct plan positions mint distinct versions and BOTH run (code = 2).
#[compio::test]
async fn duplicate_identical_dml_ops_both_apply_on_sqlite() {
    let p = paths("dup_dml");
    let be = backend(&p);
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;

    // Seed one row with code = 0.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,0,"counter"]]}
    ]}"#;
    lower_plan_and_apply(&be, seed, &registry(&[("codes", APP)]), Approval::None).await;

    // TWO byte-identical increment UPDATEs (same SET expr + same WHERE ⇒ same
    // assembled template AND same binds). Distinct plan positions must mint distinct
    // versions so BOTH net-apply rather than the second being skipped as a re-deploy.
    let dup_ir = r#"{"ir_version":1,"name":"double_bump","ops":[
        {"op":"update","table":"codes",
         "set":{"code":{"node":"binOp","op":"add",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":1}}},
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"label"},
             "rhs":{"node":"literal","value":"counter"}}},
        {"op":"update","table":"codes",
         "set":{"code":{"node":"binOp","op":"add",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":1}}},
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"label"},
             "rhs":{"node":"literal","value":"counter"}}}
    ]}"#;
    lower_plan_and_apply(&be, dup_ir, &registry(&[("codes", APP)]), Approval::None).await;

    let rows = be.actor().query("SELECT code FROM codes WHERE label = 'counter'").await.expect("probe");
    assert_eq!(
        rows[0],
        vec![Some("2".into())],
        "two byte-identical increment UPDATEs must BOTH run (distinct plan-position versions), code = 2"
    );
}

/// LOW — idempotent re-deploy on SQLite. Lower + apply a one-shot DML plan, then
/// re-apply the IDENTICAL plan: the executor must report the DML step(s) SKIPPED
/// (net-applied in the journal) and the row state must be UNCHANGED — the insert
/// did not double and the increment update did not re-run.
#[compio::test]
async fn idempotent_redeploy_of_dml_plan_on_sqlite() {
    let p = paths("idem");
    let be = backend(&p);
    lower_and_apply(&be, create_codes_table(), &registry(&[]), Approval::None).await;

    // A one-shot plan: insert one row (code = 0) + bump it by 1 (code = 1).
    let dml_ir = r#"{"ir_version":1,"name":"seed_and_bump","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,0,"idem"]]},
        {"op":"update","table":"codes",
         "set":{"code":{"node":"binOp","op":"add",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":1}}},
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"label"},
             "rhs":{"node":"literal","value":"idem"}}}
    ]}"#;

    // First deploy: both steps net-apply.
    let first = lower_plan_and_apply(&be, dml_ir, &registry(&[("codes", APP)]), Approval::None).await;
    assert_eq!(first.applied.applied.len(), 2, "first deploy applies both DML steps");
    assert!(first.applied.skipped.is_empty(), "nothing skipped on the first deploy");
    let rows = be.actor().query("SELECT code FROM codes WHERE label = 'idem'").await.expect("probe");
    assert_eq!(rows, vec![vec![Some("1".into())]], "one row, bumped once after first deploy");

    // Re-deploy the IDENTICAL plan: every DML step is net-applied already ⇒ all
    // SKIPPED, nothing re-applied, row state unchanged (no double-insert, no re-bump).
    let second = lower_plan_and_apply(&be, dml_ir, &registry(&[("codes", APP)]), Approval::None).await;
    assert!(second.applied.applied.is_empty(), "re-deploy applies NOTHING");
    assert_eq!(second.applied.skipped.len(), 2, "re-deploy skips both already-applied DML steps");
    let rows = be.actor().query("SELECT code FROM codes WHERE label = 'idem'").await.expect("probe");
    assert_eq!(
        rows,
        vec![vec![Some("1".into())]],
        "idempotent re-deploy: still exactly one row, still code = 1 (no double-insert, no re-bump)"
    );
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
/// rejected at validate-time via the support matrix — never silently applied without
/// the conflict clause.
#[compio::test]
async fn on_conflict_rejected_on_sqlite() {
    let ir = r#"{"ir_version":1,"name":"upsert","ops":[
        {"op":"insert","table":"codes","columns":["code"],"rows":[[1]],
         "onConflict":{"columns":["code"]}}
    ]}"#;
    let err = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
        None,
        None,
    )
    .expect_err("onConflict must be validate-refused on SQLite");
    let zeroship_migrate::model::load::IrLoadError::Validate(err) = err else {
        panic!("expected validate error, got {err:?}");
    };
    assert_eq!(err.code, zeroship_migrate::CODE_UNSUPPORTED);
    assert_eq!(err.kind, Some(zeroship_migrate::UnsupportedKind::Op));
    assert!(
        err.reason.contains("onConflict") && err.reason.contains("PostgreSQL-only"),
        "expected support-matrix onConflict refusal, got {err}"
    );
}

/// A batched `backfill` is PORTABLE on SQLite since PR6b — it lowers cleanly to a
/// `PlanStep::Backfill` (the SQLite batched executor consumes it, §2.3.1), no
/// longer a hard error. (The `BatchedBackfillNotPortable` error variant was
/// removed with the SQLite executor landing.) Regression: PRE-PR6b this op was
/// rejected at lower; it must now succeed.
#[compio::test]
async fn batched_backfill_portable_on_sqlite() {
    use zeroship_migrate::PlanStep;
    let ir = r#"{"ir_version":1,"name":"bf","ops":[
        {"op":"backfill","table":"codes","cursorColumn":"code","batchSize":100,
         "set":{"label":{"node":"literal","value":"x"}},"name":"fill_labels"}
    ]}"#;
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
        None,
        None,
    )
    .expect("load gate");
    let plan = author
        .lower_plan(&document, &LiveSchema::default())
        .expect("a SQLite-targeted batched backfill now lowers (PR6b)");
    assert!(
        plan.steps.iter().any(|s| matches!(s, PlanStep::Backfill(spec) if spec.name == "fill_labels")),
        "the backfill lowers to a PlanStep::Backfill on SQLite; got {:?}",
        plan.steps
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

#[compio::test]
async fn in_list_predicates_apply_identically_on_sqlite() {
    let p = paths("inlist");
    let be = backend(&p);

    let create = r#"{"ir_version":1,"name":"create_inlist_rows","ops":[
        {"op":"createTable","name":"inlist_rows","columns":[
            {"name":"status","type":"text","nullable":false},
            {"name":"in_match","type":"text","nullable":false},
            {"name":"not_in_match","type":"text","nullable":false},
            {"name":"empty_in_match","type":"text","nullable":false},
            {"name":"empty_not_in_match","type":"text","nullable":false}
        ]}
    ]}"#;
    lower_and_apply(&be, create, &registry(&[]), Approval::None).await;

    let seed = r#"{"ir_version":1,"name":"seed_inlist_rows","ops":[
        {"op":"insert","table":"inlist_rows",
         "columns":["id","created_at","updated_at","version","status","in_match","not_in_match","empty_in_match","empty_not_in_match"],
         "rows":[
            ["r1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"active","no","no","no","no"],
            ["r2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"trial","no","no","no","no"],
            ["r3","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"deleted","no","no","no","no"],
            ["r4","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"archived","no","no","no","no"]
         ]}
    ]}"#;
    lower_plan_and_apply(&be, seed, &registry(&[("inlist_rows", APP)]), Approval::None).await;

    let updates = r#"{"ir_version":1,"name":"update_inlist_rows","ops":[
        {"op":"update","table":"inlist_rows",
         "set":{"in_match":{"node":"literal","value":"yes"}},
         "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":["active","trial"],"negated":false}},
        {"op":"update","table":"inlist_rows",
         "set":{"not_in_match":{"node":"literal","value":"yes"}},
         "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":["deleted","archived"],"negated":true}},
        {"op":"update","table":"inlist_rows",
         "set":{"empty_in_match":{"node":"literal","value":"yes"}},
         "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":[],"negated":false}},
        {"op":"update","table":"inlist_rows",
         "set":{"empty_not_in_match":{"node":"literal","value":"yes"}},
         "where":{"node":"inList","expr":{"node":"colRef","name":"status"},"elems":[],"negated":true}}
    ]}"#;
    lower_plan_and_apply(&be, updates, &registry(&[("inlist_rows", APP)]), Approval::None).await;

    let rows = be
        .actor()
        .query(
            "SELECT status, in_match, not_in_match, empty_in_match, empty_not_in_match \
             FROM inlist_rows ORDER BY status",
        )
        .await
        .expect("read inList proof rows");
    assert_eq!(
        rows,
        vec![
            vec![Some("active".into()), Some("yes".into()), Some("yes".into()), Some("no".into()), Some("yes".into())],
            vec![Some("archived".into()), Some("no".into()), Some("no".into()), Some("no".into()), Some("yes".into())],
            vec![Some("deleted".into()), Some("no".into()), Some("no".into()), Some("no".into()), Some("yes".into())],
            vec![Some("trial".into()), Some("yes".into()), Some("yes".into()), Some("no".into()), Some("yes".into())],
        ],
        "SQLite inList/notIn/empty-list predicate matrix"
    );
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
    let document = zeroship_migrate::model::load::load_ir_document(
        bad,
        APP,
        zeroship_migrate::model::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
        None,
        None,
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
    let document = zeroship_migrate::model::load::load_ir_document(
        bad_ident_ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Sqlite,
        &registry(&[("codes", APP)]),
        None,
        None,
    )
    .expect("load gate");
    let err = author
        .lower_plan(&document, &LiveSchema::default())
        .expect_err("a malformed set-column identifier must be rejected at assembly");
    assert!(
        matches!(err, IrLowerError::DmlAssemble(zeroship_migrate::render::dml::DmlError::InvalidIdentifier { .. })),
        "expected InvalidIdentifier, got {err:?}"
    );
}
