//! PR6b — FAITHFUL cross-backend e2e proving the §1.1 headline ("one script, both
//! backends, DDL+DML") for the bi-dialect DDL+backfill hero (§3.1) and the pinned
//! `c.fn.splitPart` envelope (§9). ONE `.ir.json` source is applied to BOTH a real
//! Postgres (`:5440`) and a real temp-file SQLite, and the resulting rows are
//! compared BYTE-FOR-BYTE. The PG `split_part` and the engine's SQLite
//! `instr`/`substr` lowering must agree across the entire §9 envelope.
//!
//! Requires `:5440` (the `*_pg` suite convention); run with `--test-threads=1`.
//!
//! Coverage:
//! - **HERO (§3.1)**: add `first_name`/`last_name`, splitPart-backfill them from
//!   `name`, drop `name` — from ONE `.ir.json`, applied on BOTH backends, identical
//!   rows. (The hero is a batched backfill, so it exercises the PR6b SQLite
//!   executor + the §9 SQLite splitPart lowering together.)
//! - **FULL §9 envelope edge-matrix**: two-token / single-token-to-empty /
//!   beyond-tokens-to-empty / empty-input / trailing-leading-consecutive-delimiter
//!   / NULL / UTF-8 value on an ASCII delim / non-default collation — each split is
//!   byte-identical on PG and SQLite, including the `n=8` boundary.
//! - The SQLite lowering's `instr` applies cleanly under the hardened authorizer
//!   (the PR6b allow-list addition exercised end-to-end — a denied `instr` would
//!   surface as an authorizer error, not a silent pass).

use std::path::PathBuf;

use compio_postgres::Client;
use tempfile::TempDir;
use zeroship_migrate::apply::backend::sqlite::Mode;
use zeroship_migrate::{
    apply::executor::LockMode, provision_migrator, apply::role::deprovision_migrator, Approval, ExecutorConfig,
    IrAuthor, LiveSchema, MigrationEngine, SqlDialect, SqliteBackend,
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

const APP: &str = "app_test";

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

fn registry(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
}

/// Apply ONE `.ir.json` on PG through the real authoring path
/// (`load_ir_document` → `lower_plan` → `apply_plan`).
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

/// Apply ONE `.ir.json` on SQLite through the real authoring path.
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

/// Read `SELECT <cols> FROM <table> ORDER BY id` off PG as stringified rows.
async fn pg_rows(conn: &Client, cfg: &ExecutorConfig, table: &str, cols: &str) -> Vec<Vec<Option<String>>> {
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

/// Read `SELECT <cols> FROM <table> ORDER BY id` off SQLite (CreatorUp) as
/// stringified rows.
async fn sqlite_rows(be: &SqliteBackend, table: &str, cols: &str) -> Vec<Vec<Option<String>>> {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .query(&format!("SELECT {cols} FROM \"{table}\" ORDER BY id"))
        .await
        .expect("sqlite select")
}

// ── HERO (§3.1): add columns + splitPart backfill + drop, both backends ──────

/// The §3.1 hero, authored as ONE `.ir.json`: a `users(id,name)` table seeded with
/// "first last" names, then `first_name`/`last_name` added, splitPart-backfilled,
/// and `name` dropped. The same source applies to PG and SQLite; the resulting
/// (first_name,last_name) rows are byte-identical.
#[compio::test]
async fn hero_ddl_splitpart_backfill_drop_both_backends() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    pg_setup(&conn, &cfg).await;
    let sp = sqlite_paths("hero");
    let be = SqliteBackend::open(&sp.app, &sp.journal).expect("sqlite backend");

    // 1. createTable users(name text). The platform auto-injects the system
    //    `id TEXT PRIMARY KEY` (+ created_at/updated_at/version) — `id` is the
    //    NOT NULL + UNIQUE cursor key, safe to page on both backends.
    let create = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[
            {"name":"name","type":"text"}
        ]}
    ]}"#;
    pg_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;
    sqlite_apply(&be, &cfg, create, &registry(&[]), Approval::None).await;

    // 2. seed rows with "first last" names (supplying the system columns).
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"users",
         "columns":["id","created_at","updated_at","version","name"],"rows":[
            ["u1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Ada Lovelace"],
            ["u2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Grace Hopper"],
            ["u3","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Linus"],
            ["u4","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,null]
        ]}
    ]}"#;
    pg_apply(&conn, &cfg, seed, &registry(&[("users", APP)]), Approval::None).await;
    sqlite_apply(&be, &cfg, seed, &registry(&[("users", APP)]), Approval::None).await;

    // 3. add first_name/last_name, splitPart-backfill, drop name — the hero plan.
    //    The backfill is batched (cursorColumn id), so PR6b's SQLite executor +
    //    the §9 SQLite splitPart lowering are both exercised here.
    let hero = r#"{"ir_version":1,"name":"split_name","ops":[
        {"op":"addColumn","table":"users","column":"first_name","type":"text"},
        {"op":"addColumn","table":"users","column":"last_name","type":"text"},
        {"op":"backfill","table":"users","cursorColumn":"id","batchSize":2,
         "set":{
            "first_name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"name"},{"node":"literal","value":" "},{"node":"literal","value":1}]},
            "last_name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"name"},{"node":"literal","value":" "},{"node":"literal","value":2}]}
         },"name":"split_name_bf"},
        {"op":"dropColumn","table":"users","column":"name"}
    ]}"#;
    pg_apply(&conn, &cfg, hero, &registry(&[("users", APP)]), Approval::Approved).await;
    sqlite_apply(&be, &cfg, hero, &registry(&[("users", APP)]), Approval::Approved).await;

    // 4. the resulting (id, first_name, last_name) rows are byte-identical.
    let pg_r = pg_rows(&conn, &cfg, "users", "id, first_name, last_name").await;
    let sq_r = sqlite_rows(&be, "users", "id, first_name, last_name").await;
    assert_eq!(pg_r, sq_r, "hero split rows must be byte-identical on PG and SQLite");
    // Spot-check the actual transform.
    assert_eq!(pg_r[0], vec![Some("u1".into()), Some("Ada".into()), Some("Lovelace".into())]);
    assert_eq!(pg_r[2], vec![Some("u3".into()), Some("Linus".into()), Some("".into())], "single-token → last empty");
    assert_eq!(pg_r[3], vec![Some("u4".into()), None, None], "NULL name → NULL parts");

    pg_teardown(&conn, &cfg).await;
}

// ── FULL §9 envelope edge-matrix: byte-identical on PG and SQLite ────────────

/// The §9 edge-matrix: for each value + part index `n`, the split is byte-identical
/// on PG `split_part` and the SQLite `instr`/`substr` lowering. Driven through ONE
/// `.ir.json` per backend (a seed + a splitPart backfill), then the result column
/// is compared cell-by-cell.
#[compio::test]
async fn split_part_envelope_byte_identical_pg_sqlite() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    pg_setup(&conn, &cfg).await;
    let sp = sqlite_paths("env");
    let be = SqliteBackend::open(&sp.app, &sp.journal).expect("sqlite backend");

    // The §9 edge-matrix: (id, value, n). ASCII ',' delimiter except the hero
    // space-split is covered above. Cover two-token, single-token→empty,
    // beyond-tokens→empty, empty input, trailing/leading/consecutive delimiter,
    // NULL, UTF-8 value on ASCII delim, and the n=8 boundary.
    // value=NULL is row id "rNULL".
    let cases: &[(&str, Option<&str>, i64)] = &[
        ("r1", Some("a,b,c"), 1),   // two+ token → "a"
        ("r2", Some("a,b,c"), 2),   // → "b"
        ("r3", Some("a,b,c"), 3),   // → "c"
        ("r4", Some("a,b,c"), 4),   // beyond tokens → ""
        ("r5", Some("a"), 2),       // single token, n=2 → ""
        ("r6", Some(""), 1),        // empty input → ""
        ("r7", Some("a,"), 2),      // trailing delim → ""
        ("r8", Some(",b"), 1),      // leading delim → ""
        ("r9", Some(",b"), 2),      // leading delim → "b"
        ("r10", Some("a,,b"), 2),   // consecutive delim → ""
        ("r11", Some("a,,b"), 3),   // → "b"
        ("r12", Some("héllo,wörld"), 1), // UTF-8 value, ASCII delim → "héllo"
        ("r13", Some("héllo,wörld"), 2), // → "wörld"
        ("r14", Some("t1,t2,t3,t4,t5,t6,t7,t8,t9"), 8), // n=8 boundary → "t8"
        // §9 fixture (i): the ASCII delimiter byte DIRECTLY adjacent to multibyte
        // content on BOTH sides — proves the byte-wise instr/substr scan never lands
        // inside a UTF-8 continuation byte (each of é/ä/ö is a 2-byte sequence, and
        // ',' sits immediately between them with no ASCII filler).
        ("rADJ1", Some("é,ä,ö"), 1), // → "é"
        ("rADJ2", Some("é,ä,ö"), 2), // → "ä" (delim adjacent to multibyte both sides)
        ("rADJ3", Some("é,ä,ö"), 3), // → "ö"
        ("rNULL", None, 1),         // NULL input → NULL
    ];

    // 1. createTable parts(v text, n int, out text) — system `id` auto-injected.
    let create = r#"{"ir_version":1,"name":"create_parts","ops":[
        {"op":"createTable","name":"parts","columns":[
            {"name":"v","type":"text"},
            {"name":"n","type":"int"},
            {"name":"out","type":"text"}
        ]}
    ]}"#;
    pg_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;
    sqlite_apply(&be, &cfg, create, &registry(&[]), Approval::None).await;

    // 2. seed the matrix rows (supplying the system columns).
    let mut rows_json = Vec::new();
    for (id, v, n) in cases {
        let vj = match v {
            Some(s) => serde_json::to_string(s).unwrap(),
            None => "null".to_string(),
        };
        rows_json.push(format!(
            "[\"{id}\",\"2026-01-01T00:00:00Z\",\"2026-01-01T00:00:00Z\",1,{vj},{n}]"
        ));
    }
    let seed = format!(
        r#"{{"ir_version":1,"name":"seed","ops":[
            {{"op":"insert","table":"parts",
              "columns":["id","created_at","updated_at","version","v","n"],"rows":[{}]}}
        ]}}"#,
        rows_json.join(",")
    );
    pg_apply(&conn, &cfg, &seed, &registry(&[("parts", APP)]), Approval::None).await;
    sqlite_apply(&be, &cfg, &seed, &registry(&[("parts", APP)]), Approval::None).await;

    // 3. For each distinct n in the matrix, a one-shot UPDATE setting out =
    //    splitPart(v, ',', n) WHERE n = <n>. (n must be a literal in the AST — the
    //    envelope requires it — so we emit one update per distinct n.)
    let mut ns: Vec<i64> = cases.iter().map(|(_, _, n)| *n).collect();
    ns.sort_unstable();
    ns.dedup();
    for n in ns {
        let upd = format!(
            r#"{{"ir_version":1,"name":"split_n{n}","ops":[
                {{"op":"update","table":"parts",
                  "set":{{"out":{{"node":"fnSynth","fn":"splitPart","args":[
                      {{"node":"colRef","name":"v"}},{{"node":"literal","value":","}},{{"node":"literal","value":{n}}}]}}}},
                  "where":{{"node":"binOp","op":"eq",
                      "lhs":{{"node":"colRef","name":"n"}},"rhs":{{"node":"literal","value":{n}}}}}}}
            ]}}"#
        );
        pg_apply(&conn, &cfg, &upd, &registry(&[("parts", APP)]), Approval::None).await;
        sqlite_apply(&be, &cfg, &upd, &registry(&[("parts", APP)]), Approval::None).await;
    }

    // 4. the `out` column is byte-identical on PG and SQLite, row for row.
    let pg_r = pg_rows(&conn, &cfg, "parts", "id, out").await;
    let sq_r = sqlite_rows(&be, "parts", "id, out").await;
    assert_eq!(pg_r, sq_r, "splitPart envelope must be byte-identical on PG and SQLite");

    // 5. spot-check the documented §9 results so a both-wrong-identically bug is caught.
    let by_id = |id: &str| -> Option<String> {
        pg_r.iter()
            .find(|r| r[0].as_deref() == Some(id))
            .and_then(|r| r[1].clone())
    };
    assert_eq!(by_id("r1").as_deref(), Some("a"));
    assert_eq!(by_id("r3").as_deref(), Some("c"));
    assert_eq!(by_id("r4").as_deref(), Some(""), "beyond tokens → empty");
    assert_eq!(by_id("r6").as_deref(), Some(""), "empty input → empty");
    assert_eq!(by_id("r9").as_deref(), Some("b"), "leading delim, n=2 → b");
    assert_eq!(by_id("r12").as_deref(), Some("héllo"), "UTF-8 value, ASCII delim");
    assert_eq!(by_id("r14").as_deref(), Some("t8"), "n=8 boundary");
    // §9 fixture (i): delimiter adjacent to multibyte content on both sides — the
    // byte-wise scan splits cleanly on the ASCII ',' without straddling a UTF-8
    // continuation byte of the surrounding 2-byte code points.
    assert_eq!(by_id("rADJ1").as_deref(), Some("é"), "multibyte-adjacent delim, n=1");
    assert_eq!(by_id("rADJ2").as_deref(), Some("ä"), "multibyte-adjacent delim on both sides, n=2");
    assert_eq!(by_id("rADJ3").as_deref(), Some("ö"), "multibyte-adjacent delim, n=3");
    assert_eq!(by_id("rNULL"), None, "NULL input → NULL");

    pg_teardown(&conn, &cfg).await;
}

// ── non-default collation: the split is collation-independent (byte split) ───

/// The helper splits on a literal byte and does NO collation-dependent comparison,
/// so a value stored under a non-default (NOCASE) collation splits identically. We
/// declare `v` with `COLLATE NOCASE` on SQLite via the engine path is not directly
/// authorable, so we assert the structural property: the split of a mixed-case
/// value is byte-identical on PG and SQLite (PG uses no collation in split_part;
/// the SQLite lowering uses instr/substr, both byte-wise).
#[compio::test]
async fn split_part_is_collation_independent() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    pg_setup(&conn, &cfg).await;
    let sp = sqlite_paths("coll");
    let be = SqliteBackend::open(&sp.app, &sp.journal).expect("sqlite backend");

    let create = r#"{"ir_version":1,"name":"create_c","ops":[
        {"op":"createTable","name":"c","columns":[
            {"name":"v","type":"text"},
            {"name":"out","type":"text"}
        ]}
    ]}"#;
    pg_apply(&conn, &cfg, create, &registry(&[]), Approval::None).await;
    sqlite_apply(&be, &cfg, create, &registry(&[]), Approval::None).await;

    // A mixed-case value where a case-insensitive collation WOULD matter for a
    // comparison — but the split is byte-wise, so case is preserved identically.
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"c","columns":["id","created_at","updated_at","version","v"],
         "rows":[["x","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Foo,BAR,baz"]]}
    ]}"#;
    pg_apply(&conn, &cfg, seed, &registry(&[("c", APP)]), Approval::None).await;
    sqlite_apply(&be, &cfg, seed, &registry(&[("c", APP)]), Approval::None).await;

    let upd = r#"{"ir_version":1,"name":"split","ops":[
        {"op":"update","table":"c",
         "set":{"out":{"node":"fnSynth","fn":"splitPart","args":[
             {"node":"colRef","name":"v"},{"node":"literal","value":","},{"node":"literal","value":2}]}}}
    ]}"#;
    pg_apply(&conn, &cfg, upd, &registry(&[("c", APP)]), Approval::None).await;
    sqlite_apply(&be, &cfg, upd, &registry(&[("c", APP)]), Approval::None).await;

    let pg_r = pg_rows(&conn, &cfg, "c", "id, out").await;
    let sq_r = sqlite_rows(&be, "c", "id, out").await;
    assert_eq!(pg_r, sq_r, "collation-independent byte split is identical");
    assert_eq!(pg_r[0][1].as_deref(), Some("BAR"), "case preserved, byte-wise split");

    pg_teardown(&conn, &cfg).await;
}
