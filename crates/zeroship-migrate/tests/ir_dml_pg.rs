//! §PR6a — FAITHFUL e2e for the creator DML surface on **real Postgres** (`:5440`),
//! driven through the AUTHORING path: a `.ir.json` → the REAL load-gate + the
//! creator-DML assembler (`IrAuthor::lower_plan`) → `MigrationEngine::apply_plan`
//! → a real PG schema under the least-priv migrator role. No shims, no hand-built
//! `PlanStep::Dml` (that PR0 path is covered by `apply_plan_pg.rs`); here the
//! statement + binds come from the ASSEMBLER, completing PR0 test (9)'s
//! authoring-path obligation.
//!
//! Coverage:
//! - **insert → update → delete one-shot** authored as IR ops apply on real PG and
//!   the row state proves the transform happened;
//! - **bind-safety**: an `insert` whose value carries SQL metacharacters is stored
//!   verbatim (native `$n`) and cannot alter the statement shape;
//! - **`insert { onConflict }` renders natively on PG** (an upsert really upserts);
//! - **a batched `backfill`** runs through the existing PG `BackfillSpec` executor
//!   (resumable, crash-safe) — the assembler-rendered set/filter applies.
//!
//! Requires `:5440` (the `*_pg` suite convention); run with `--test-threads=1`.

use compio_postgres::Client;
use zeroship_migrate::{
    apply::executor::LockMode,
    frontend::record_migration_to_ir_unsandboxed,
    model::migration::MigrationId,
    apply::role::deprovision_migrator, provision_migrator, Approval, ExecutorConfig, IrAuthor,
    LiveSchema, MigrationBackend, MigrationEngine, MigrationIr, PolicyProfile, SqlDialect,
    resolve_create_table_policy,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn q(schema: &str) -> String {
    format!("\"{}\"", schema.replace('"', "\"\""))
}

fn registry(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_secs() as i64
}

const APP: &str = "app_test";

fn resolve_ir_json(ir: &str) -> String {
    let raw: MigrationIr = serde_json::from_str(ir).expect("test IR parses");
    let resolved =
        resolve_create_table_policy(&raw, &PolicyProfile::confined()).expect("test IR resolves");
    serde_json::to_string(&resolved).expect("resolved test IR serializes")
}

/// Author a `.ir.json` (the deploying app `APP`) → REAL load-gate + assembler
/// (`lower_plan`) → `apply_plan` on real PG. Asserts apply success.
async fn author_and_apply(
    conn: &Client,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
    approval: Approval,
) -> zeroship_migrate::engine::DeclarativeDeployOutcome {
    let ir = resolve_ir_json(ir);
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::model::load::load_ir_document(
        &ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
    )
    .expect("load gate");
    let plan = author.lower_plan(&document, &LiveSchema::default()).expect("lower the IR plan on PG");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &plan.steps,
            approval,
            &zeroship_migrate::PostgresBackend::new(conn),
            cfg,
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("apply the authored DML plan on PG")
}

async fn load_recorded_ir_and_apply(
    conn: &Client,
    cfg: &ExecutorConfig,
    ir: &zeroship_migrate::MigrationIr,
    reg: &std::collections::BTreeMap<String, String>,
) {
    let bytes = serde_json::to_string(ir).expect("recorded IR serializes");
    let document = zeroship_migrate::model::load::load_ir_document(
        &bytes,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
    )
    .expect("load recorded IR through the PG gate");
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let plan = author
        .lower_plan(&document, &LiveSchema::default())
        .expect("render recorded fnSynth IR on PG");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &plan.steps,
            Approval::None,
            &zeroship_migrate::PostgresBackend::new(conn),
            cfg,
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("apply recorded fnSynth IR on PG");
}

#[compio::test]
async fn ir_authored_insert_update_delete_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    // DDL: a real table (the migrator role creates it).
    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
            {"op":"createTable","name":"codes","columns":[
                {"name":"code","type":"int","nullable":false,"unique":true},
                {"name":"label","type":"text"}
            ]}
        ]}"#;
    let _ = s;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

    // INSERT two rows through the assembler.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[
            ["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,200,"ok"],
            ["c2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,404,null]
         ]}
    ]}"#;
    author_and_apply(&conn, &cfg, seed, &registry(&[("codes", APP)]), Approval::None).await;
    let s = q(&cfg.project_schema);
    let n = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.codes"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(n, 2, "two rows seeded via the assembler on PG");

    // UPDATE: coalesce the null label where code > 300.
    let update = r#"{"ir_version":1,"name":"fixup","ops":[
        {"op":"update","table":"codes",
         "set":{"label":{"node":"fnCall","fn":"coalesce","args":[
             {"node":"colRef","name":"label"},
             {"node":"literal","value":"unknown"}]}},
         "where":{"node":"binOp","op":"gt",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":300}}}
    ]}"#;
    author_and_apply(&conn, &cfg, update, &registry(&[("codes", APP)]), Approval::None).await;
    let label: String = conn
        .query_one(&format!("SELECT label FROM {s}.codes WHERE code = 404"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(label, "unknown", "the assembler-authored one-shot UPDATE ran on PG");

    // DELETE (destructive ⇒ needs approval).
    let delete = r#"{"ir_version":1,"name":"prune","ops":[
        {"op":"delete","table":"codes",
         "where":{"node":"binOp","op":"eq",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":200}}}
    ]}"#;
    author_and_apply(&conn, &cfg, delete, &registry(&[("codes", APP)]), Approval::Approved).await;
    let n = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.codes"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(n, 1, "one row pruned via the assembler-authored DELETE");

    teardown(&conn, &cfg).await;
}

/// Bind-safety on real PG: a metacharacter-laden insert value is stored verbatim
/// (native `$n` bind) — the table survives and the value is byte-identical.
#[compio::test]
async fn ir_bind_safety_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false,"unique":true},
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

    let hostile = "x'); DROP TABLE codes; --";
    let seed = format!(
        r#"{{"ir_version":1,"name":"seed","ops":[
            {{"op":"insert","table":"codes",
             "columns":["id","created_at","updated_at","version","code","label"],
             "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,1,{hostile:?}]]}}
        ]}}"#
    );
    author_and_apply(&conn, &cfg, &seed, &registry(&[("codes", APP)]), Approval::None).await;

    // The table survived; the value is verbatim.
    let label: String = conn
        .query_one(&format!("SELECT label FROM {s}.codes WHERE code = 1"), &[])
        .await
        .expect("table survived the injection attempt")
        .get(0);
    assert_eq!(label, hostile, "metacharacter value stored verbatim, not interpreted");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn recorded_fnsynth_symbol_insert_applies_db_evaluated_values_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pgcrypto")
        .await
        .expect("pgcrypto/gen_random_uuid is available");
    setup(&conn, &cfg).await;

    let create = r#"{"ir_version":1,"name":"create_events","ops":[
        {"op":"createTable","name":"events","columns":[
            {"name":"kind","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

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
    let symbol_ir = record_migration_to_ir_unsandboxed(symbol_src, APP, "pg_fnsynth_symbol")
        .expect("record symbol-form fnSynth insert");
    let explicit_ir = record_migration_to_ir_unsandboxed(explicit_src, APP, "pg_fnsynth_explicit")
        .expect("record explicit cFn insert");
    assert_eq!(
        symbol_ir.ops, explicit_ir.ops,
        "native symbol form and cFn form must record byte-identical ops"
    );

    compio::time::sleep(std::time::Duration::from_secs(2)).await;
    let apply_start = unix_secs();
    load_recorded_ir_and_apply(&conn, &cfg, &symbol_ir, &registry(&[("events", APP)])).await;
    let apply_end = unix_secs();

    let s = q(&cfg.project_schema);
    let row = conn
        .query_one(
            &format!("SELECT id, extract(epoch FROM created_at)::bigint FROM {s}.events WHERE kind = 'symbol'"),
            &[],
        )
        .await
        .expect("read fnSynth-applied row");
    let id: String = row.get(0);
    let created_epoch: i64 = row.get(1);

    uuid::Uuid::parse_str(&id)
        .unwrap_or_else(|e| panic!("id must be a DB-generated uuid, got {id:?}: {e}"));
    assert!(
        created_epoch >= recorded_at + 1,
        "created_at must be evaluated at apply time, not record time: record={recorded_at}, stored={created_epoch}"
    );
    assert!(
        (apply_start - 1..=apply_end + 5).contains(&created_epoch),
        "created_at must land in the apply window: apply={apply_start}..{apply_end}, stored={created_epoch}"
    );

    teardown(&conn, &cfg).await;
}

/// `insert { onConflict }` renders natively on PG and really upserts: a second
/// insert on a conflicting key DOES UPDATE the row (the PG-only path, §9).
#[compio::test]
async fn ir_on_conflict_upserts_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false,"unique":true},
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

    // First insert: code=1, label='first'.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,1,"first"]]}
    ]}"#;
    author_and_apply(&conn, &cfg, seed, &registry(&[("codes", APP)]), Approval::None).await;

    // Upsert: insert the same code with ON CONFLICT (code) DO UPDATE SET label='second'.
    let upsert = r#"{"ir_version":1,"name":"upsert","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[["c1b","2026-01-02T00:00:00Z","2026-01-02T00:00:00Z",1,1,"second"]],
         "onConflict":{"columns":["code"],"doUpdate":{"label":"second"}}}
    ]}"#;
    author_and_apply(&conn, &cfg, upsert, &registry(&[("codes", APP)]), Approval::None).await;

    let label: String = conn
        .query_one(&format!("SELECT label FROM {s}.codes WHERE code = 1"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(label, "second", "ON CONFLICT DO UPDATE upserted the row on PG");
    let n = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.codes"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(n, 1, "the upsert updated in place, no duplicate row");

    teardown(&conn, &cfg).await;
}

/// A batched `backfill` authored as IR runs through the existing PG `BackfillSpec`
/// executor (resumable, crash-safe) — the assembler-rendered `set`/`filter` apply.
#[compio::test]
async fn ir_batched_backfill_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false,"unique":true},
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

    // Seed rows with NULL labels.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[
            ["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,1,null],
            ["c2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,2,null],
            ["c3","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,3,null]
         ]}
    ]}"#;
    author_and_apply(&conn, &cfg, seed, &registry(&[("codes", APP)]), Approval::None).await;

    // Backfill: set label = 'filled' where label IS NULL, paged by `code`.
    let backfill = r#"{"ir_version":1,"name":"bf","ops":[
        {"op":"backfill","table":"codes","cursorColumn":"code","batchSize":2,
         "set":{"label":{"node":"literal","value":"filled"}},
         "filter":{"node":"unaryOp","op":"isNull","operand":{"node":"colRef","name":"label"}},
         "name":"fill_labels"}
    ]}"#;
    // A backfill mutates data ⇒ Approval::Approved.
    author_and_apply(&conn, &cfg, backfill, &registry(&[("codes", APP)]), Approval::Approved).await;

    let filled = conn
        .query_one(
            &format!("SELECT count(*)::bigint FROM {s}.codes WHERE label = 'filled'"),
            &[],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(filled, 3, "the assembler-authored batched backfill filled all 3 rows on PG");

    teardown(&conn, &cfg).await;
}

/// **PR6a `c.fn.concatWs` NULL-skip apply on real PG (§9) — the byte-identity peer
/// of the SQLite `concat_ws_null_skip_applies_byte_identical_on_sqlite`.** PG's
/// native `concat_ws` SKIPS NULL arguments, so `concat_ws('-', '1', NULL)` = `'1'`
/// (no trailing delimiter). This pins the PG render to the EXACT same expected value
/// (`'1'`) the SQLite head-trim fold must produce — the two legs assert the same
/// byte string, proving the SQLite lowering is byte-identical to PG.
#[compio::test]
async fn ir_concat_ws_null_skip_applies_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false,"unique":true},
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

    // Seed: code=1, label=NULL.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"codes",
         "columns":["id","created_at","updated_at","version","code","label"],
         "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,1,null]]}
    ]}"#;
    author_and_apply(&conn, &cfg, seed, &registry(&[("codes", APP)]), Approval::None).await;

    // UPDATE: label = concatWs('-', cast(code as text), label)  where code = 1.
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
    author_and_apply(&conn, &cfg, ir, &registry(&[("codes", APP)]), Approval::None).await;
    let label: String = conn
        .query_one(&format!("SELECT label FROM {s}.codes WHERE code = 1"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        label, "1",
        "PG concat_ws('-', '1', NULL) = '1' — the SAME byte string the SQLite fold yields"
    );

    teardown(&conn, &cfg).await;
}

/// **PR6a rule-(c) column-scoping at the apply/render seam (§3.3.1.1(c)).** An
/// `update` whose SET RHS references a column that does NOT exist on the live
/// target table must be rejected with the STRUCTURED `UNSUPPORTED { kind: "expr" }`
/// AuthoringError at lower/apply — BEFORE the template is assembled — NOT surface
/// as a raw DB "column does not exist" error at execution. This is the faithful
/// production-path proof: the live `LiveSchema` is built from REAL PG introspection
/// (`snapshot_schema`, the same facts `apply_bundle_ir_migrations` seeds), then the
/// authoring path (`lower_plan`) is driven with it. A NOTHING-applied reject.
#[compio::test]
async fn ir_unresolved_colref_rejected_at_apply_seam_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    // DDL: create `codes` (code, label).
    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false,"unique":true},
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

    // Introspect the LIVE schema the SAME way the production deploy path does, so
    // the lower sees the real live columns of `codes` (system fields + code/label).
    let backend = zeroship_migrate::PostgresBackend::new(&conn);
    let live = backend.snapshot_schema(&cfg).await.expect("introspect live schema");
    let live_schema = LiveSchema {
        tables: live.tables.keys().cloned().collect(),
        unique_indexes: live
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect(),
        table_snapshots: live.tables.clone(),
        table_ownership: live.tables.keys().map(|t| (t.clone(), APP.to_string())).collect(),
        sqlite_schemas: std::collections::BTreeMap::new(),
    };
    assert!(
        live_schema.table_snapshots.contains_key("codes"),
        "introspection must surface the live `codes` table"
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
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::model::load::load_ir_document(
        bad,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        &registry(&[("codes", APP)]),
        None,
    )
    .expect("load gate (DML ColRef resolution is an apply-seam reject, not a load reject)");
    let err = author
        .lower_plan(&document, &live_schema)
        .expect_err("an unresolved ColRef must be rejected at the resolved apply seam, pre-assembly");
    match err {
        zeroship_migrate::IrLowerError::DmlValidate(e) => {
            assert_eq!(e.code, zeroship_migrate::CODE_UNSUPPORTED, "structured (c) reject");
            assert_eq!(
                e.kind,
                Some(zeroship_migrate::UnsupportedKind::Expr),
                "rule (c) is an expr-kind capability-boundary reject"
            );
        }
        other => panic!("expected DmlValidate UNSUPPORTED expr, got {other:?}"),
    }

    teardown(&conn, &cfg).await;
}

/// HIGH (PG side) — `op.del(table, where, {limit})` deletes EXACTLY `limit`
/// matching rows on real PG via the ctid-subquery form (PG never supported a bare
/// `DELETE … LIMIT n`). Pins the PG leg of the portable del+limit lowering green
/// alongside the SQLite rowid-subquery fix.
#[compio::test]
async fn ir_del_with_limit_deletes_exactly_n_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    // No UNIQUE on code here — five rows all match the WHERE so the limit bites.
    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false},
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

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
    author_and_apply(&conn, &cfg, seed, &registry(&[("codes", APP)]), Approval::None).await;

    let delete = r#"{"ir_version":1,"name":"prune_limited","ops":[
        {"op":"delete","table":"codes",
         "where":{"node":"binOp","op":"ge",
             "lhs":{"node":"colRef","name":"code"},
             "rhs":{"node":"literal","value":1}},
         "limit":2}
    ]}"#;
    author_and_apply(&conn, &cfg, delete, &registry(&[("codes", APP)]), Approval::Approved).await;

    let n = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.codes"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(n, 3, "del{{limit:2}} via the ctid subquery removes EXACTLY 2 of 5 matching rows on PG");

    teardown(&conn, &cfg).await;
}

/// LOW — idempotent re-deploy on real PG. Apply a one-shot DML plan (insert + an
/// increment update), then re-apply the IDENTICAL plan: the executor must report
/// the DML steps SKIPPED (net-applied) and the row state unchanged (no
/// double-insert, no re-bump).
#[compio::test]
async fn ir_idempotent_redeploy_of_dml_plan_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let s = q(&cfg.project_schema);

    let create = r#"{"ir_version":1,"name":"create_codes","ops":[
        {"op":"createTable","name":"codes","columns":[
            {"name":"code","type":"int","nullable":false},
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;

    let dml = r#"{"ir_version":1,"name":"seed_and_bump","ops":[
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

    let first = author_and_apply(&conn, &cfg, dml, &registry(&[("codes", APP)]), Approval::None).await;
    assert_eq!(first.applied.applied.len(), 2, "first deploy applies both DML steps");
    assert!(first.applied.skipped.is_empty(), "nothing skipped on the first deploy");
    let code1: i32 = conn
        .query_one(&format!("SELECT code FROM {s}.codes WHERE label = 'idem'"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(code1, 1, "one row, bumped once after first deploy");

    // Re-deploy the identical plan: all DML steps net-applied ⇒ all skipped.
    let second = author_and_apply(&conn, &cfg, dml, &registry(&[("codes", APP)]), Approval::None).await;
    assert!(second.applied.applied.is_empty(), "re-deploy applies NOTHING on PG");
    assert_eq!(second.applied.skipped.len(), 2, "re-deploy skips both already-applied DML steps");
    let n = conn
        .query_one(&format!("SELECT count(*)::bigint FROM {s}.codes WHERE label = 'idem'"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(n, 1, "idempotent re-deploy: still exactly one row (no double-insert)");
    let code2: i32 = conn
        .query_one(&format!("SELECT code FROM {s}.codes WHERE label = 'idem'"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(code2, 1, "idempotent re-deploy: still code = 1 (the increment did not re-run)");

    teardown(&conn, &cfg).await;
}

// Touch MigrationId to keep the import meaningful across refactors.
#[allow(dead_code)]
fn _id() -> MigrationId {
    MigrationId::generate()
}
