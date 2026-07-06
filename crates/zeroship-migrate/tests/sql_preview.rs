//! **PR14 — the OFFLINE `--sql` plan preview gate.**
//!
//! These tests run with NO DB connection (no `_pg`/`_sqlite` suffix, not gated on
//! `MIGRATE_REQUIRE_DB`): the preview's whole point is to render the SQL the engine
//! WOULD run WITHOUT a live DB. They assert:
//!
//! 1. GOLDEN: a representative migration set (createTable + addColumn + a guarded
//!    addColumn + a one-shot insert/update + an online renameColumn) renders to a
//!    byte-stable golden for all preview dialects
//!    (`tests/golden/sql_preview_{pg,sqlite,mysql}.txt`).
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

use zeroship_migrate::render::lower::{IrAuthor, LiveSchema};
use zeroship_migrate::PlanStep;
use zeroship_migrate::render::sql_preview::{
    render_ir_json_sql, render_plan_sql, render_set_sql, PreviewOpts, RUNTIME_RESOLVED,
};
use zeroship_migrate::{
    resolve_create_table_policy, MigrationIr, PolicyProfile,
};
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
    {"op":"addColumn","table":"codes","column":"flag","type":"boolean","nullable":true,"existenceGuard":"ifNotExists"},
    {"op":"createIndex","table":"codes","name":"codes_label_idx","columns":[{"kind":"column","name":"label"}]},
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

/// Same representative set for MySQL, excluding `renameColumn`: MySQL declares
/// rename unsupported in this phase, so preview must validate-refuse it rather
/// than print a runtime-resolved label.
const REPRESENTATIVE_IR_MYSQL: &str = r#"{
  "ir_version": 1,
  "name": "representative",
  "ops": [
    {"op":"createTable","name":"codes","columns":[
      {"name":"code","type":"int","nullable":false,"unique":true},
      {"name":"label","type":"text"}
    ]},
    {"op":"addColumn","table":"codes","column":"note","type":"text","nullable":true},
    {"op":"addColumn","table":"codes","column":"flag","type":"boolean","nullable":true,"existenceGuard":"ifNotExists"},
    {"op":"createIndex","table":"codes","name":"codes_label_idx","columns":[{"kind":"column","name":"label"}]},
    {"op":"insert","table":"codes",
      "columns":["id","created_at","updated_at","version","code","label"],
      "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,200,"ok"]]},
    {"op":"update","table":"codes",
      "set":{"label":{"node":"literal","value":"x"}},
      "where":{"node":"binOp","op":"gt",
        "lhs":{"node":"colRef","name":"code"},
        "rhs":{"node":"literal","value":300}}}
  ]
}"#;

/// MySQL-specific render proof: the portable IR pieces MySQL can render in phase 1
/// lower to valid MySQL 8 DDL/DML without opening a database.
const MYSQL_FEATURE_IR: &str = r#"{
  "ir_version": 1,
  "name": "mysql_feature",
  "ops": [
    {"op":"createTable","name":"teams","columns":[
      {"name":"id","type":"int","nullable":false,"identity":{"always":false}},
      {"name":"name","type":"text","nullable":false},
      {"name":"name_lc","type":"string",
        "generated":{"expr":{"node":"fnCall","fn":"lower","args":[{"node":"colRef","name":"name"}]},"stored":true}}
    ],"primaryKey":["id"]},
    {"op":"createTable","name":"members","columns":[
      {"name":"id","type":"int","nullable":false,"identity":{"always":false}},
      {"name":"team_id","type":"int","nullable":false},
      {"name":"email","type":"text","nullable":false}
    ],"primaryKey":["id"],"constraints":[
      {"name":"members_team_fk","kind":{"kind":"fk","columns":["team_id"],
        "referencesTable":"teams","referencesColumns":["id"],"onDelete":"cascade"}}
    ],"indexes":[{"name":"members_team_id_idx","columns":[{"kind":"column","name":"team_id"}]}]},
    {"op":"createView","name":"active_teams","replace":true,
      "query":{"kind":"structured","select":{"from":{"name":"teams"},
        "projection":[{"kind":"colRef","name":"id"},{"kind":"colRef","name":"name"}],
        "where":{"node":"unaryOp","op":"isNotNull","operand":{"node":"colRef","name":"name"}}}}},
    {"op":"insert","table":"teams",
      "columns":["id","name"],
      "rows":[[1,"Core"]]}
  ]
}"#;

fn opts() -> PreviewOpts {
    PreviewOpts { default_schema: "public".to_string(), owner_app: "app_preview".to_string() }
}

/// Render the representative IR for a dialect through the offline IR preview.
fn render_representative(dialect: SqlDialect) -> String {
    let ir = if dialect == SqlDialect::Mysql {
        REPRESENTATIVE_IR_MYSQL
    } else {
        REPRESENTATIVE_IR
    };
    let ir = resolve_ir_json(ir);
    render_ir_json_sql(&ir, dialect, &opts())
        .expect("representative IR renders offline")
}

fn resolve_ir_json(ir: &str) -> String {
    let raw: MigrationIr = serde_json::from_str(ir).expect("preview fixture IR parses");
    let resolved =
        resolve_create_table_policy(&raw, &PolicyProfile::confined()).expect("preview fixture IR resolves");
    serde_json::to_string(&resolved).expect("resolved preview fixture serializes")
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

#[test]
fn golden_mysql() {
    assert_golden("sql_preview_mysql.txt", &render_representative(SqlDialect::Mysql));
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

#[test]
fn faithful_to_lowered_sql_mysql() {
    faithful_to_lowered_sql(SqlDialect::Mysql);
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
    let ir_json = resolve_ir_json(ir_json);
    let ir: MigrationIr = serde_json::from_str(&ir_json).unwrap();
    let author = IrAuthor::new("public", "app_preview", dialect);
    let steps = author.lower_steps(&ir, &LiveSchema::default()).expect("lowers offline");

    let preview = render_ir_json_sql(&ir_json, dialect, &opts()).expect("renders offline");

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

#[test]
fn mysql_feature_preview_renders_mysql8_sql() {
    let ir = resolve_ir_json(MYSQL_FEATURE_IR);
    let out = render_ir_json_sql(&ir, SqlDialect::Mysql, &opts())
        .expect("MySQL feature fixture renders offline");
    assert!(out.contains("CREATE TABLE `public`.`teams`"), "{out}");
    assert!(out.contains("`id` INT AUTO_INCREMENT PRIMARY KEY"), "{out}");
    assert!(
        out.contains("GENERATED ALWAYS AS (lower(`name`)) STORED"),
        "{out}"
    );
    assert!(out.contains("CREATE INDEX `members_team_id_idx`"), "{out}");
    assert!(
        out.contains(
            "ALTER TABLE `public`.`members` ADD CONSTRAINT `members_team_fk` \
             FOREIGN KEY (`team_id`) REFERENCES `public`.`teams` (`id`) \
             ON DELETE CASCADE"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "CREATE OR REPLACE VIEW `public`.`active_teams` AS SELECT `id`, `name` \
             FROM `public`.`teams` WHERE (`name` IS NOT NULL)"
        ),
        "{out}"
    );
    assert!(out.contains("VALUES (?, ?)"), "{out}");
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
    let ir = resolve_ir_json(ir);
    let out = render_ir_json_sql(&ir, SqlDialect::Postgres, &opts()).expect("renders offline");
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
      {"op":"addColumn","table":"codes","column":"flag","type":"boolean","nullable":true,"existenceGuard":"ifNotExists"}
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
    let plans = zeroship_migrate::plan::loader::load_dir(&dir).expect("loads .sql offline");
    let out = render_set_sql(&plans, SqlDialect::Postgres, &opts());
    assert!(out.contains("CREATE TABLE widgets (id text primary key)"), "{out}");
    assert!(out.contains("-- preview:"), "carries a summary line:\n{out}");
    std::fs::remove_dir_all(&dir).ok();
}

/// MED-1 — HONESTY ON THE RAW `.sql` LEG. Operator-authored raw `.sql` is rendered
/// VERBATIM, never dialect-transformed. A PG-only `.sql` (`SERIAL`) rendered under
/// `--dialect sqlite` must therefore NOT be captioned with a bare `(dialect: sqlite)`
/// claim — that would mislead an operator reviewing a SQLite go-live into thinking
/// the PG SQL had been lowered for SQLite. The header must carry the verbatim/NOT-
/// transformed disclaimer instead, while the body stays byte-verbatim.
#[test]
fn raw_sql_caption_does_not_claim_a_transformed_dialect() {
    let dir = tempdir_with(&[(
        "V0001__legacy.sql",
        "CREATE TABLE legacy (id SERIAL PRIMARY KEY, name text);\n",
    )]);
    let plans = zeroship_migrate::plan::loader::load_dir(&dir).expect("loads .sql offline");
    // Render the PG-only raw SQL under the SQLITE dialect request.
    let out = render_set_sql(&plans, SqlDialect::Sqlite, &opts());

    // The PG SQL is shown VERBATIM (the SERIAL never became INTEGER / AUTOINCREMENT).
    assert!(out.contains("id SERIAL PRIMARY KEY"), "raw SQL must be verbatim:\n{out}");

    // CRITICAL: no bare `(dialect: sqlite)` claim anywhere — neither the doc header
    // nor the per-plan header may assert the SQL was lowered for SQLite.
    assert!(
        !out.contains("(dialect: sqlite)"),
        "raw .sql must NOT be captioned with a transformed-dialect claim:\n{out}"
    );
    // It DOES surface the honest verbatim/NOT-transformed disclaimer.
    assert!(
        out.contains("NOT dialect-transformed"),
        "raw .sql header must disclose it is verbatim / not transformed:\n{out}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// RENDER SUCCEEDS WITHOUT A DSN (truth-in-advertising, LOW-1). Scrubbing
/// `DATABASE_URL` and asserting `is_ok()` proves only that the render does not
/// REQUIRE a DSN env var — it does NOT prove the absence of a hard-coded connect
/// (a path dialing a fixed host would still pass here). Named honestly for what it
/// proves. The LOAD-BEARING offline-proof is `cli_plan_prints_and_exits_zero_offline`
/// below: it runs the real binary under a scrubbed env, so a stray connect to any
/// host would fail or hang the subprocess.
#[test]
fn render_succeeds_without_a_dsn() {
    // Scrub any inherited DSN so the render cannot lean on an env-provided DSN.
    std::env::remove_var("DATABASE_URL");
    let representative = resolve_ir_json(REPRESENTATIVE_IR);
    let representative_mysql = resolve_ir_json(REPRESENTATIVE_IR_MYSQL);
    let pg = render_ir_json_sql(&representative, SqlDialect::Postgres, &opts());
    let sqlite = render_ir_json_sql(&representative, SqlDialect::Sqlite, &opts());
    let mysql = render_ir_json_sql(&representative_mysql, SqlDialect::Mysql, &opts());
    assert!(
        pg.is_ok() && sqlite.is_ok() && mysql.is_ok(),
        "offline render must not need a DSN"
    );
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

/// LOW-2 — `render_plan_sql` over a PG `OnlineRename(PgExpandContract)` plan. This is
/// the public-API entrypoint for a hand-built rename plan (no CLI path feeds an
/// OnlineRename step). It locks the no-fabrication contract for the rename render
/// surface: the expand/contract ADDITIVE DDL must appear ONLY as `--`-comment lines
/// under a `-- [runtime-resolved]` label, and NO bare executable rename SQL (no
/// `ALTER … RENAME`, no `CREATE TRIGGER`, no uncommented expand DDL) may leak.
#[test]
fn render_plan_sql_online_rename_is_labeled_never_fabricated() {
    use zeroship_migrate::{PlanStep, RenameStep};
    use zeroship_migrate::{ExpandContractAuthor, OnlineIntent};

    // Author a REAL PG expand-contract plan via the same author the engine uses, so
    // the test feeds the genuine E1..C2 + backfill shape (never a synthetic stub).
    let ec = ExpandContractAuthor::new("public", "app_preview")
        .author(&OnlineIntent::RenameColumn {
            table: "codes".to_string(),
            from: "label".to_string(),
            to: "display_name".to_string(),
            ty: "text".to_string(),
        })
        .expect("expand-contract author lowers the rename");
    let rename = RenameStep::PgExpandContract(ec);

    // Borrow a real, fully-formed AppliedPlan via the offline `.sql` loader, then swap
    // its single DDL step for the OnlineRename step (the only piece under test).
    let dir = tempdir_with(&[("V0001__seed.sql", "CREATE TABLE codes (id text primary key);\n")]);
    let mut plan = zeroship_migrate::plan::loader::load_dir(&dir)
        .expect("loads .sql offline")
        .pop()
        .expect("one plan");
    plan.steps = vec![PlanStep::OnlineRename(rename)];

    let out = render_plan_sql(&plan, SqlDialect::Postgres, &opts());

    // The rename is labeled runtime-resolved (the backfill + cutover depend on live state).
    assert!(
        out.contains(RUNTIME_RESOLVED) && out.contains("expand-contract"),
        "online rename must be labeled runtime-resolved:\n{out}"
    );

    // The expand/contract additive DDL IS surfaced (the E1 `ADD COLUMN` / the E2
    // dual-write `CREATE … TRIGGER`) — but it must appear ONLY inside `--` comment
    // lines, never as a bare executable statement.
    assert!(out.contains("ADD COLUMN"), "the additive expand DDL should be surfaced:\n{out}");

    // THE no-fabrication invariant: every non-blank line is a comment. No bare
    // executable rename SQL (no uncommented ALTER/CREATE TRIGGER/RENAME) may leak —
    // if any did, it would appear as a non-`--` line and trip this loop.
    for line in out.lines() {
        let l = line.trim_start();
        if l.is_empty() || l.starts_with("--") {
            continue;
        }
        panic!("bare executable SQL leaked from an online-rename render: {line:?}\n{out}");
    }
    // Belt-and-suspenders: the lines that carry the rename mechanics are comments.
    for needle in ["ADD COLUMN", "CREATE OR REPLACE FUNCTION", "CREATE", "TRIGGER"] {
        for line in out.lines().filter(|l| l.contains(needle)) {
            assert!(
                line.trim_start().starts_with("--"),
                "rename mechanic {needle:?} must be a comment, not executable: {line:?}\n{out}"
            );
        }
    }
    std::fs::remove_dir_all(&dir).ok();
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
    let representative = resolve_ir_json(REPRESENTATIVE_IR);
    let dir = tempdir_with(&[("001_create.ir.json", &representative)]);
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

#[test]
fn cli_plan_mysql_engine_prints_and_exits_zero_offline() {
    let mysql_feature = resolve_ir_json(MYSQL_FEATURE_IR);
    let dir = tempdir_with(&[("001_create.ir.json", &mysql_feature)]);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_zeroship-migrate"))
        .args(["--engine", "mysql", "plan", "--dir"])
        .arg(&dir)
        .env_remove("DATABASE_URL")
        .output()
        .expect("spawn zeroship-migrate");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "plan must exit 0 offline; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("(dialect: mysql)"), "stdout:\n{stdout}");
    assert!(stdout.contains("AUTO_INCREMENT"), "stdout:\n{stdout}");
    assert!(stdout.contains("CREATE OR REPLACE VIEW"), "stdout:\n{stdout}");
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
