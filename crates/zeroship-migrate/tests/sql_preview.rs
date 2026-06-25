//! **PR14 — the OFFLINE `--sql` plan preview gate.**
//!
//! These tests run with NO DB connection (no `_pg`/`_sqlite` suffix, not gated on
//! `MIGRATE_REQUIRE_DB`): the preview's whole point is to render the SQL the engine
//! WOULD run WITHOUT a live DB. They assert:
//!
//! 1. GOLDEN: a representative migration set (createTable + addColumn + a guarded
//!    addColumn + a one-shot insert/update + an online renameColumn) renders to a
//!    byte-stable golden for BOTH dialects (`tests/golden/sql_preview_{pg,sqlite}.txt`).
//! 2. FAITHFULNESS: the rendered DDL/DML is byte-identical to the SQL the engine
//!    actually lowers (`IrAuthor::lower_steps` `Migration.up` / `PlanStep::Dml.template`)
//!    — the preview is a surfacing layer, NOT a re-implementation.
//! 3. NO FABRICATION: online-rename / backfill / SQLite-rebuild-only / guarded ops
//!    produce a `-- [runtime-resolved]` label, never invented SQL (and a guarded op's
//!    bare DDL never gains a fabricated `IF [NOT] EXISTS`).
//! 4. NO DB CONNECTION: the render path opens no socket (proven by running it with NO
//!    DSN env set and no DB reachable — a connection attempt would error/hang).
//!
//! Regenerate the goldens with `UPDATE_PREVIEW_GOLDENS=1 cargo test -p zeroship-migrate
//! --test sql_preview`.

use zeroship_migrate::ir_author::{IrAuthor, LiveSchema};
use zeroship_migrate::plan::PlanStep;
use zeroship_migrate::sql_preview::{
    render_ir_json_sql, render_plan_sql, render_set_sql, PreviewOpts, RUNTIME_RESOLVED,
};
use zeroship_migrate::MigrationIr;
use zeroship_schema::query::SqlDialect;

/// The representative IR exercising every renderable op + the honest-boundary
/// witnesses (a guarded addColumn, a one-shot insert/update, an online rename).
const REPRESENTATIVE_IR: &str = r#"{
  "ir_version": 1,
  "name": "representative",
  "ops": [
    {"op":"createTable","name":"codes","columns":[
      {"name":"code","type":"int","nullable":false,"unique":true},
      {"name":"label","type":"text"}
    ]},
    {"op":"addColumn","table":"codes","column":"note","type":"text","nullable":true},
    {"op":"addColumn","table":"codes","column":"flag","type":"bool","nullable":true,"existenceGuard":"ifNotExists"},
    {"op":"createIndex","table":"codes","name":"codes_label_idx","columns":["label"]},
    {"op":"insert","table":"codes",
      "columns":["id","created_at","updated_at","version","code","label"],
      "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,200,"ok"]]},
    {"op":"update","table":"codes",
      "set":{"label":{"node":"literal","value":"x"}},
      "where":{"node":"binOp","op":"gt",
        "lhs":{"node":"colRef","name":"code"},
        "rhs":{"node":"literal","value":300}}},
    {"op":"renameColumn","table":"codes","from":"label","to":"display_name","type":"text"}
  ]
}"#;

fn opts() -> PreviewOpts {
    PreviewOpts { default_schema: "public".to_string(), owner_app: "app_preview".to_string() }
}

/// Render the representative IR for a dialect through the offline IR preview.
fn render_representative(dialect: SqlDialect) -> String {
    render_ir_json_sql(REPRESENTATIVE_IR, dialect, &opts())
        .expect("representative IR renders offline")
}

/// Golden-compare helper: write-or-assert against a committed golden file.
fn assert_golden(name: &str, actual: &str) {
    let path = format!("{}/tests/golden/{name}", env!("CARGO_MANIFEST_DIR"));
    if std::env::var("UPDATE_PREVIEW_GOLDENS").is_ok() {
        std::fs::create_dir_all(format!("{}/tests/golden", env!("CARGO_MANIFEST_DIR"))).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read golden {path}: {e} (run UPDATE_PREVIEW_GOLDENS=1)"));
    assert_eq!(actual, expected, "preview golden drift for {name}");
}

#[test]
fn golden_pg() {
    assert_golden("sql_preview_pg.txt", &render_representative(SqlDialect::Postgres));
}

#[test]
fn golden_sqlite() {
    assert_golden("sql_preview_sqlite.txt", &render_representative(SqlDialect::Sqlite));
}

/// FAITHFULNESS — every rendered DDL/DML statement is byte-identical to the SQL the
/// engine actually lowers (`Migration.up` / `PlanStep::Dml.template`). We lower the
/// DB-INDEPENDENT ops (drop the online rename, which is runtime-resolved) and assert
/// each lowered statement appears verbatim in the preview text. This proves the
/// preview surfaces the lowered SQL rather than re-implementing a renderer.
#[test]
fn faithful_to_lowered_sql_pg() {
    faithful_to_lowered_sql(SqlDialect::Postgres);
}

#[test]
fn faithful_to_lowered_sql_sqlite() {
    faithful_to_lowered_sql(SqlDialect::Sqlite);
}

fn faithful_to_lowered_sql(dialect: SqlDialect) {
    // An IR with ONLY the DB-independent ops (so `lower_steps` succeeds end-to-end).
    let ir_json = r#"{
      "ir_version": 1,
      "name": "faithful",
      "ops": [
        {"op":"createTable","name":"codes","columns":[
          {"name":"code","type":"int","nullable":false,"unique":true},
          {"name":"label","type":"text"}
        ]},
        {"op":"addColumn","table":"codes","column":"note","type":"text","nullable":true},
        {"op":"insert","table":"codes",
          "columns":["id","created_at","updated_at","version","code","label"],
          "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,200,"ok"]]}
      ]
    }"#;
    let ir: MigrationIr = serde_json::from_str(ir_json).unwrap();
    let author = IrAuthor::new("public", "app_preview", dialect);
    let steps = author.lower_steps(&ir, &LiveSchema::default()).expect("lowers offline");

    let preview = render_ir_json_sql(ir_json, dialect, &opts()).expect("renders offline");

    for step in &steps {
        match step {
            PlanStep::Ddl(m) => {
                // The engine's exact lowered `up` (no trailing `;` normalization here)
                // must appear verbatim in the preview output.
                let body = m.up.trim_end().trim_end_matches(';');
                assert!(
                    preview.contains(body),
                    "preview missing the engine-lowered DDL for {dialect:?}:\n--- lowered ---\n{body}\n--- preview ---\n{preview}"
                );
            }
            PlanStep::Dml { template, .. } => {
                let body = template.trim_end().trim_end_matches(';');
                assert!(
                    preview.contains(body),
                    "preview missing the engine-lowered DML template for {dialect:?}:\n{body}"
                );
            }
            other => panic!("unexpected step in DB-independent IR: {other:?}"),
        }
    }
}

/// NO FABRICATION (the load-bearing witness) — an online `renameColumn` is labeled
/// `[runtime-resolved]` and the preview contains NO fabricated rename SQL
/// (no `CREATE TABLE`-rebuild, no dual-write trigger, no `ALTER … RENAME`).
#[test]
fn online_rename_is_labeled_never_fabricated() {
    for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
        let ir = r#"{"ir_version":1,"name":"r","ops":[
          {"op":"renameColumn","table":"codes","from":"label","to":"display_name","type":"text"}
        ]}"#;
        let out = render_ir_json_sql(ir, dialect, &opts()).expect("renders offline");
        assert!(
            out.contains(RUNTIME_RESOLVED) && out.contains("online rename"),
            "rename must be labeled runtime-resolved for {dialect:?}:\n{out}"
        );
        // No fabricated rename mechanics.
        assert!(!out.contains("RENAME"), "must not fabricate ALTER … RENAME:\n{out}");
        assert!(!out.contains("CREATE TRIGGER"), "must not fabricate a dual-write trigger:\n{out}");
        // The only statement lines are comments — there is no executable rename SQL.
        for line in out.lines() {
            let l = line.trim_start();
            if l.is_empty() || l.starts_with("--") {
                continue;
            }
            panic!("fabricated executable SQL for an online rename in {dialect:?}: {line:?}\n{out}");
        }
    }
}

/// NO FABRICATION — a `backfill` op is labeled `[runtime-resolved]`, never rendered
/// as an executable batched statement stream.
#[test]
fn backfill_is_labeled_never_fabricated() {
    // Backfill needs a `set`/`where`; a minimal valid op.
    let ir = r#"{"ir_version":1,"name":"bf","ops":[
      {"op":"createTable","name":"codes","columns":[{"name":"code","type":"int"}]},
      {"op":"backfill","table":"codes","name":"bf_codes","cursorColumn":"code","batchSize":100,
        "set":{"code":{"node":"literal","value":0}},
        "filter":{"node":"binOp","op":"gt",
          "lhs":{"node":"colRef","name":"code"},
          "rhs":{"node":"literal","value":1000}}}
    ]}"#;
    let out = render_ir_json_sql(ir, SqlDialect::Postgres, &opts()).expect("renders offline");
    assert!(
        out.contains(RUNTIME_RESOLVED) && out.contains("backfill"),
        "backfill must be labeled runtime-resolved:\n{out}"
    );
}

/// NO FABRICATION — a guarded (`ifNotExists`) addColumn carries the runtime catalog-
/// probe label AND its bare DDL has NO invented `IF NOT EXISTS` clause (the guard is
/// a runtime probe, not a native clause).
#[test]
fn guarded_op_labeled_and_bare_ddl_has_no_fabricated_clause() {
    let ir = r#"{"ir_version":1,"name":"g","ops":[
      {"op":"addColumn","table":"codes","column":"flag","type":"bool","nullable":true,"existenceGuard":"ifNotExists"}
    ]}"#;
    let out = render_ir_json_sql(ir, SqlDialect::Postgres, &opts()).expect("renders offline");
    assert!(
        out.contains(RUNTIME_RESOLVED) && out.contains("catalog-probed"),
        "guarded addColumn must carry the catalog-probe label:\n{out}"
    );
    // The bare ADD COLUMN statement must NOT have a fabricated IF NOT EXISTS on the
    // ALTER. (The label line mentions ifNotExists; the STATEMENT line must not.)
    let alter_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("ALTER TABLE"))
        .expect("a bare ALTER TABLE ADD COLUMN statement is rendered");
    assert!(
        !alter_line.to_ascii_uppercase().contains("IF NOT EXISTS"),
        "guarded addColumn must render BARE DDL (no fabricated IF NOT EXISTS): {alter_line:?}"
    );
}

/// A `.sql` (Flyway/dbmate) directory loads + renders WITHOUT a DB via `load_dir` +
/// `render_set_sql`. Proves the `.sql` leg is offline and the verbatim body is shown.
#[test]
fn sql_dir_renders_offline() {
    let dir = tempdir_with(&[
        ("V0001__widgets.sql", "CREATE TABLE widgets (id text primary key);\n"),
    ]);
    let plans = zeroship_migrate::loader::load_dir(&dir).expect("loads .sql offline");
    let out = render_set_sql(&plans, SqlDialect::Postgres, &opts());
    assert!(out.contains("CREATE TABLE widgets (id text primary key)"), "{out}");
    assert!(out.contains("-- preview:"), "carries a summary line:\n{out}");
    std::fs::remove_dir_all(&dir).ok();
}

/// NO DB CONNECTION — render the representative IR with NO `DATABASE_URL` set and no
/// DB reachable. The offline render path must succeed without opening a socket; if it
/// ever connected, this would error or hang. (A timing-free structural proof: the
/// `render_ir_json_sql` API takes only bytes + dialect + opts — there is no DSN
/// parameter — and it returns synchronously here.)
#[test]
fn render_opens_no_db_connection() {
    // Scrub any inherited DSN so a stray connect would fail loudly.
    std::env::remove_var("DATABASE_URL");
    let pg = render_ir_json_sql(REPRESENTATIVE_IR, SqlDialect::Postgres, &opts());
    let sqlite = render_ir_json_sql(REPRESENTATIVE_IR, SqlDialect::Sqlite, &opts());
    assert!(pg.is_ok() && sqlite.is_ok(), "offline render must not need a DB");
}

/// `render_plan_sql` — the single-plan renderer (symmetric with `render_set_sql`,
/// which the `.sql`-dir test exercises). Lower a DB-independent IR to one
/// `AppliedPlan` offline, render it, and assert the per-plan header + the
/// engine-lowered DDL surface verbatim (a surfacing layer, not a re-render).
#[test]
fn render_plan_sql_surfaces_lowered_ddl_offline() {
    let ir_json = r#"{
      "ir_version": 1,
      "name": "single",
      "ops": [
        {"op":"createTable","name":"widgets","columns":[
          {"name":"sku","type":"text","nullable":false,"unique":true}
        ]}
      ]
    }"#;
    let ir: MigrationIr = serde_json::from_str(ir_json).unwrap();
    let author = IrAuthor::new("public", "app_preview", SqlDialect::Postgres);
    let plan = author
        .lower_plan(&ir, &LiveSchema::default())
        .expect("DB-independent IR lowers offline");

    let out = render_plan_sql(&plan, SqlDialect::Postgres, &opts());
    assert!(out.contains("-- plan"), "carries the per-plan header:\n{out}");
    assert!(out.contains("(dialect: postgres)"), "labels the dialect:\n{out}");

    // Every lowered DDL body appears verbatim — faithfulness to the engine's lowering.
    let steps = author.lower_steps(&ir, &LiveSchema::default()).unwrap();
    for step in &steps {
        if let PlanStep::Ddl(m) = step {
            let body = m.up.trim_end().trim_end_matches(';');
            assert!(out.contains(body), "render_plan_sql missing lowered DDL:\n{body}\n--\n{out}");
        }
    }
}

/// A malformed `.ir.json` is a hard error (the CLI maps this to a non-zero exit).
#[test]
fn malformed_ir_is_error() {
    let err = render_ir_json_sql("{ not json", SqlDialect::Postgres, &opts());
    assert!(err.is_err(), "malformed IR must be an error");
}

/// CLI SMOKE (no DB) — `plan --dir <fixture> --dialect pg` prints the preview and
/// exits 0 WITHOUT a DSN. Proves the subcommand dispatch is offline (it returns
/// BEFORE the DSN-bearing RunConfig is built).
#[test]
fn cli_plan_prints_and_exits_zero_offline() {
    let dir = tempdir_with(&[("001_create.ir.json", REPRESENTATIVE_IR)]);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_zeroship-migrate"))
        .args(["plan", "--dir"])
        .arg(&dir)
        .args(["--dialect", "pg"])
        // Deliberately NO --database-url and scrub the env DSN: a connect would fail.
        .env_remove("DATABASE_URL")
        .output()
        .expect("spawn zeroship-migrate");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "plan must exit 0 offline; stderr:\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("offline SQL preview") || stdout.contains("-- plan"), "stdout:\n{stdout}");
    assert!(stdout.contains(RUNTIME_RESOLVED), "the rename must be labeled:\n{stdout}");
    assert!(stdout.contains("CREATE TABLE"), "renderable DDL must be shown:\n{stdout}");
    std::fs::remove_dir_all(&dir).ok();
}

/// CLI SMOKE — a missing/empty dir is a non-zero exit (the operator's error signal).
#[test]
fn cli_plan_empty_dir_exits_nonzero() {
    let dir = tempdir_with(&[]); // no artifacts
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_zeroship-migrate"))
        .args(["plan", "--dir"])
        .arg(&dir)
        .args(["--dialect", "pg"])
        .env_remove("DATABASE_URL")
        .output()
        .expect("spawn zeroship-migrate");
    assert!(!out.status.success(), "an empty dir must be a non-zero refusal");
    std::fs::remove_dir_all(&dir).ok();
}

/// Create a unique temp dir seeded with `(filename, contents)` files; caller removes.
fn tempdir_with(files: &[(&str, &str)]) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "zsm_preview_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    for (name, body) in files {
        std::fs::write(base.join(name), body).unwrap();
    }
    base
}
