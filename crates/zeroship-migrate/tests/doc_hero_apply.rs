//! CI DOC-EXAMPLE GATE (PR8, Rust leg) — the bi-dialect HERO example documented
//! in `docs/reference/migrate-op-dsl.md` must actually APPLY, byte-identically,
//! on a real Postgres AND a real SQLite. A future engine change that rots the
//! documented "one script, both backends" claim FAILS CI here.
//!
//! The test EXTRACTS the hero `.ir.json` straight from the doc's "Appendix: the
//! hero example as IR" fenced ```json block (so the doc and the applied artifact
//! cannot drift — change the doc IR shape and this test re-applies the new shape;
//! break the engine and the apply fails). It then runs the doc's `up()` sequence
//! (createTable people → seed "first last" names → the addColumn/splitPart-backfill
//! /dropColumn hero) on BOTH backends through the REAL authoring path
//! (`load_ir_document` → `lower_plan` → `apply_plan`) and asserts the resulting
//! `(id, first_name, last_name)` rows are byte-for-byte equal.
//!
//! Requires `:5440` (the `*_pg` suite convention); run with `--test-threads=1`.
//! When `MIGRATE_REQUIRE_DB` is set (CI MUST set it) an unreachable PG is a HARD
//! failure, not a silent skip — masking a rotted doc would defeat the gate.
//!
//! REGRESSION WITNESS: `doc_hero_ir_is_actually_extracted_and_well_formed` proves
//! the extractor really pulls a non-trivial hero IR out of the doc (the exact
//! shape — addColumn ×2 + a splitPart backfill + dropColumn). If a future edit
//! deletes the appendix or guts the IR block, that witness fails RED, so the
//! apply gate above can never silently become a no-op over an empty/absent IR.

use std::path::PathBuf;

use compio_postgres::Client;
use tempfile::TempDir;
use zeroship_migrate::apply::backend::sqlite::Mode;
use zeroship_migrate::{
    apply::executor::LockMode, provision_migrator, apply::role::deprovision_migrator, Approval, ExecutorConfig,
    IrAuthor, LiveSchema, MigrationEngine, SqlDialect, SqliteBackend,
};

const DOC: &str = include_str!("../../../docs/reference/migrate-op-dsl.md");

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

const APP: &str = "app_doc_hero";

/// Pull the fenced ```json block from the hero `.ir.json` appendix.
fn extract_hero_ir(md: &str) -> String {
    let appendix = "## Appendix: the hero example as IR";
    let marker = "```json\n";
    let appendix_start = md.find(appendix).expect("doc must contain the hero IR appendix");
    let appendix_rest = &md[appendix_start..];
    let start = appendix_rest
        .find(marker)
        .expect("hero appendix must contain a ```json hero IR block");
    let rest = &appendix_rest[start + marker.len()..];
    let end = rest.find("```").expect("the ```json block must be closed");
    rest[..end].trim().to_string()
}

fn registry(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
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

async fn pg() -> Option<Client> {
    match compio_postgres::connect(&dsn(), compio_postgres::NoTls).await {
        Ok((client, conn)) => {
            compio::runtime::spawn(async move {
                let _ = conn.run().await;
            })
            .detach();
            Some(client)
        }
        Err(e) => {
            assert!(
                std::env::var("MIGRATE_REQUIRE_DB").is_err(),
                "MIGRATE_REQUIRE_DB is set but zeroship_migrate_test on :5440 is unreachable: {e}"
            );
            eprintln!("SKIP doc_hero_apply: zeroship_migrate_test :5440 unreachable ({e})");
            None
        }
    }
}

async fn pg_setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
}

async fn pg_teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

async fn pg_apply(
    conn: &Client,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
    approval: Approval,
) {
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
    )
    .expect("PG load gate");
    let plan = author.lower_plan(&document, &LiveSchema::default()).expect("PG lower");
    MigrationEngine::new()
        .apply_plan(
            &plan.steps,
            approval,
            &zeroship_migrate::PostgresBackend::new(conn),
            cfg,
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("PG apply_plan");
}

struct SqlitePaths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn sqlite_paths(tag: &str) -> SqlitePaths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    SqlitePaths { _dir: dir, app, journal }
}

async fn sqlite_apply(
    be: &SqliteBackend,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
    approval: Approval,
) {
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Sqlite);
    let document = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Sqlite,
        reg,
        None,
    )
    .expect("SQLite load gate");
    let plan = author.lower_plan(&document, &LiveSchema::default()).expect("SQLite lower");
    MigrationEngine::new()
        .apply_plan(&plan.steps, approval, be, cfg, APP, LockMode::Acquire)
        .await
        .expect("SQLite apply_plan");
}

async fn pg_rows(
    conn: &Client,
    cfg: &ExecutorConfig,
    table: &str,
    cols: &str,
) -> Vec<Vec<Option<String>>> {
    let s = format!("\"{}\"", cfg.project_schema.replace('"', "\"\""));
    let sql = format!("SELECT {cols} FROM {s}.\"{table}\" ORDER BY id");
    let rows = conn.query(&sql, &[]).await.expect("pg select");
    rows.iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.try_get::<_, Option<String>>(i).unwrap_or(None))
                .collect()
        })
        .collect()
}

async fn sqlite_rows(be: &SqliteBackend, table: &str, cols: &str) -> Vec<Vec<Option<String>>> {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .query(&format!("SELECT {cols} FROM \"{table}\" ORDER BY id"))
        .await
        .expect("sqlite select")
}

// ── REGRESSION WITNESS: the extractor really pulls a non-trivial hero IR ──────

#[test]
fn doc_hero_ir_is_actually_extracted_and_well_formed() {
    let ir = extract_hero_ir(DOC);
    // It is valid JSON.
    let v: serde_json::Value = serde_json::from_str(&ir).expect("doc hero IR must be valid JSON");
    // It is the documented hero shape — addColumn ×2 + a splitPart backfill +
    // dropColumn over `people`. If a future edit guts the appendix, this fails RED
    // so the apply gate can never become a silent no-op over an empty IR.
    let ops = v["ops"].as_array().expect("ops array");
    assert_eq!(ops.len(), 4, "hero IR must carry exactly the 4 documented ops; got {}", ops.len());
    assert_eq!(ops[0]["op"], "addColumn");
    assert_eq!(ops[1]["op"], "addColumn");
    assert_eq!(ops[2]["op"], "backfill");
    assert_eq!(ops[3]["op"], "dropColumn");
    assert_eq!(
        ops[2]["set"]["first_name"]["fn"], "splitPart",
        "the documented backfill must use the c.fn.splitPart helper"
    );
    assert_eq!(ops[3]["table"], "people");
}

// ── THE GATE: the documented hero applies byte-identically on PG and SQLite ───

#[compio::test]
async fn doc_hero_applies_byte_identically_on_both_backends() {
    let Some(conn) = pg().await else { return };
    let cfg = cfg_for(&token());
    pg_setup(&conn, &cfg).await;
    let sp = sqlite_paths("doc_hero");
    let be = SqliteBackend::open(&sp.app, &sp.journal).expect("sqlite backend");

    // The doc's hero operates on `people(name)`; mirror the doc's `up()` preamble:
    // create the table, then seed "first last" names (supplying the system cols).
    let create = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[{"name":"name","type":"text"}]}
    ]}"#;
    pg_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;
    sqlite_apply(&be, &cfg, create, &registry(&[]), Approval::None).await;

    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"people",
         "columns":["id","created_at","updated_at","version","name"],"rows":[
            ["p1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Ada Lovelace"],
            ["p2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Grace Hopper"],
            ["p3","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Linus"],
            ["p4","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,null]
        ]}
    ]}"#;
    pg_apply(&conn, &cfg, seed, &registry(&[("people", APP)]), Approval::None).await;
    sqlite_apply(&be, &cfg, seed, &registry(&[("people", APP)]), Approval::None).await;

    // THE DOCUMENTED HERO IR — extracted from the doc appendix, applied on both
    // backends (the backfill mutates data ⇒ Approval::Approved).
    let hero = extract_hero_ir(DOC);
    pg_apply(&conn, &cfg, &hero, &registry(&[("people", APP)]), Approval::Approved).await;
    sqlite_apply(&be, &cfg, &hero, &registry(&[("people", APP)]), Approval::Approved).await;

    // Byte-identical rows on both backends — the documented end state.
    let pg_r = pg_rows(&conn, &cfg, "people", "id, first_name, last_name").await;
    let sq_r = sqlite_rows(&be, "people", "id, first_name, last_name").await;
    assert_eq!(pg_r, sq_r, "the documented hero must apply byte-identically on PG and SQLite");
    assert_eq!(pg_r[0], vec![Some("p1".into()), Some("Ada".into()), Some("Lovelace".into())]);
    assert_eq!(
        pg_r[2],
        vec![Some("p3".into()), Some("Linus".into()), Some("".into())],
        "single-token name → empty last_name"
    );
    assert_eq!(pg_r[3], vec![Some("p4".into()), None, None], "NULL name → NULL parts");

    pg_teardown(&conn, &cfg).await;
}
