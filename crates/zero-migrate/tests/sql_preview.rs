//! **The OFFLINE `--sql` plan preview gate.**
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
//! Regenerate the goldens with `UPDATE_PREVIEW_GOLDENS=1 cargo test -p zero-migrate
//! --test sql_preview`.

mod support;

use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::render::sql_preview::{
    render_ir_envelope_sql, render_plan_sql, render_set_sql, PreviewOpts, RUNTIME_RESOLVED,
};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::PlanStep;
use zero_migrate::{resolve_create_table_policy, MigrationIr};

/// The representative IR exercising every renderable op + the honest-boundary
/// witnesses (a guarded addColumn, a one-shot insert/update, and an online rename
/// on a separate existing table).
const REPRESENTATIVE_IR: &str = r#"{
  "ir_version": 1,
  "name": "representative",
  "ops": [
    {"op":"createTable","name":"codes","columns":[
      {"name":"code","type":"int","nullable":false,"unique":true},
      {"name":"label","type":{"string":{"length":255}}}
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
    {"op":"renameColumn","table":"legacy_codes","from":"label","to":"display_name","type":"text"}
  ]
}"#;

/// Same representative set for `MySQL`, excluding `renameColumn`: `MySQL` declares
/// rename unsupported in this phase, so preview must validate-refuse it rather
/// than print a runtime-resolved label.
const REPRESENTATIVE_IR_MYSQL: &str = r#"{
  "ir_version": 1,
  "name": "representative",
  "ops": [
    {"op":"createTable","name":"codes","columns":[
      {"name":"code","type":"int","nullable":false,"unique":true},
      {"name":"label","type":{"string":{"length":255}}}
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

/// MySQL-specific render proof: the portable IR pieces `MySQL` supports lower to
/// valid `MySQL` 8 DDL/DML without opening a database.
const MYSQL_FEATURE_IR: &str = r#"{
  "ir_version": 1,
  "name": "mysql_feature",
  "ops": [
    {"op":"createTable","name":"teams","columns":[
      {"name":"id","type":"int","nullable":false,"identity":{"always":false}},
      {"name":"name","type":"text","nullable":false},
      {"name":"name_lc","type":"text",
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
    PreviewOpts {
        default_schema: "public".to_string(),
        owner_app: "app_preview".to_string(),
        effective_policy: support::confined_charter(),
    }
}

/// Render the representative IR for a dialect through the offline IR preview.
fn render_representative(dialect: SqlDialect) -> String {
    let ir = if dialect == SqlDialect::Mysql {
        REPRESENTATIVE_IR_MYSQL
    } else {
        REPRESENTATIVE_IR
    };
    let ir = resolve_envelope_json(ir);
    render_ir_envelope_sql(&ir, dialect, &opts()).expect("representative IR renders offline")
}

fn resolve_envelope_json(ir: &str) -> String {
    let raw: MigrationIr = serde_json::from_str(ir).expect("preview fixture IR parses");
    let resolved = resolve_create_table_policy(&raw, &support::confined_charter(), "public")
        .expect("preview fixture IR resolves");
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
    assert_golden(
        "sql_preview_pg.txt",
        &render_representative(SqlDialect::Postgres),
    );
}

#[test]
fn golden_sqlite() {
    assert_golden(
        "sql_preview_sqlite.txt",
        &render_representative(SqlDialect::Sqlite),
    );
}

#[test]
fn golden_mysql() {
    assert_golden(
        "sql_preview_mysql.txt",
        &render_representative(SqlDialect::Mysql),
    );
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
    let envelope_json = r#"{
      "ir_version": 1,
      "name": "faithful",
      "ops": [
        {"op":"createTable","name":"codes","columns":[
          {"name":"code","type":"int","nullable":false,"unique":true},
          {"name":"label","type":{"string":{"length":255}}}
        ]},
        {"op":"addColumn","table":"codes","column":"note","type":"text","nullable":true},
        {"op":"insert","table":"codes",
          "columns":["id","created_at","updated_at","version","code","label"],
          "rows":[["c1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,200,"ok"]]}
      ]
    }"#;
    let envelope_json = resolve_envelope_json(envelope_json);
    let ir: MigrationIr = serde_json::from_str(&envelope_json).unwrap();
    let author = IrAuthor::new(
        "public",
        "app_preview",
        dialect,
        &support::confined_charter(),
    );
    let steps = author
        .lower_steps(&ir, &LiveSchema::default())
        .expect("lowers offline");

    let preview =
        render_ir_envelope_sql(&envelope_json, dialect, &opts()).expect("renders offline");

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
    let ir = resolve_envelope_json(MYSQL_FEATURE_IR);
    let out = render_ir_envelope_sql(&ir, SqlDialect::Mysql, &opts())
        .expect("MySQL feature fixture renders offline");
    assert!(
        !out.contains(RUNTIME_RESOLVED),
        "an FK to an earlier table in the same envelope is fully previewable: {out}"
    );
    assert!(out.contains("CREATE TABLE `public`.`teams`"), "{out}");
    assert!(out.contains("`id` INT AUTO_INCREMENT PRIMARY KEY"), "{out}");
    assert!(
        out.contains("GENERATED ALWAYS AS (lower(`name`)) STORED"),
        "{out}"
    );
    assert!(
        out.contains("KEY `members_team_id_idx` (`team_id`)"),
        "the MySQL supporting index must be visible inline in the CREATE preview: {out}"
    );
    assert!(
        out.contains(
            "CONSTRAINT `members_team_fk` FOREIGN KEY (`team_id`) \
             REFERENCES `public`.`teams` (`id`) ON DELETE CASCADE"
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
        let out = render_ir_envelope_sql(ir, dialect, &opts()).expect("renders offline");
        assert!(
            out.contains(RUNTIME_RESOLVED) && out.contains("online rename"),
            "rename must be labeled runtime-resolved for {dialect:?}:\n{out}"
        );
        // No fabricated rename mechanics.
        assert!(
            !out.contains("RENAME"),
            "must not fabricate ALTER … RENAME:\n{out}"
        );
        assert!(
            !out.contains("CREATE TRIGGER"),
            "must not fabricate a dual-write trigger:\n{out}"
        );
        // The only statement lines are comments — there is no executable rename SQL.
        for line in out.lines() {
            let l = line.trim_start();
            if l.is_empty() || l.starts_with("--") {
                continue;
            }
            panic!(
                "fabricated executable SQL for an online rename in {dialect:?}: {line:?}\n{out}"
            );
        }
    }
}

/// NO FABRICATION — a `backfill` op is labeled `[runtime-resolved]`, never rendered
/// as an executable batched statement stream.
#[test]
fn backfill_is_labeled_never_fabricated() {
    // Backfill needs a `set`/`where`; a minimal valid op.
    let ir = r#"{"ir_version":1,"name":"bf","ops":[
      {"op":"createTable","name":"codes","columns":[
        {"name":"code","type":"int","nullable":false,"unique":true},
        {"name":"migrated","type":"int","nullable":true}
      ]},
      {"op":"backfill","table":"codes","name":"bf_codes","cursorColumns":["code"],
       "cursorStability":{"mode":"guardUpdates"},"batchSize":100,
        "set":{"migrated":{"node":"literal","value":0}},
        "filter":{"node":"binOp","op":"gt",
          "lhs":{"node":"colRef","name":"code"},
          "rhs":{"node":"literal","value":1000}}}
    ]}"#;
    let ir = resolve_envelope_json(ir);
    let out = render_ir_envelope_sql(&ir, SqlDialect::Postgres, &opts()).expect("renders offline");
    assert!(
        out.contains(RUNTIME_RESOLVED) && out.contains("backfill"),
        "backfill must be labeled runtime-resolved:\n{out}"
    );
    assert!(
        out.contains("cursorColumns [\"code\"]")
            && out.contains("cursor stability guardUpdates")
            && out.contains("concurrent inserts require"),
        "preview must expose the tuple, stability mode, and bounded-cohort caveat:\n{out}"
    );
}

#[test]
fn external_cursor_invariant_is_prominent_in_preview() {
    let ir = r#"{"ir_version":1,"name":"bf_external","ops":[
      {"op":"createTable","name":"codes","columns":[
        {"name":"code","type":"int","nullable":false,"unique":true},
        {"name":"migrated","type":"int","nullable":true}
      ]},
      {"op":"backfill","table":"codes","name":"bf_codes","cursorColumns":["code"],
       "cursorStability":{"mode":"externalInvariant","name":"writers_freeze_code_keys"},
       "batchSize":100,"set":{"migrated":{"node":"literal","value":0}}}
    ]}"#;
    let ir = resolve_envelope_json(ir);
    let out = render_ir_envelope_sql(&ir, SqlDialect::Postgres, &opts()).expect("preview");
    assert!(
        out.contains("CURSOR STABILITY EXTERNAL INVARIANT")
            && out.contains("writers_freeze_code_keys")
            && out.contains("explicit destructive approval required"),
        "external cursor invariant must be operator-prominent:\n{out}"
    );
}

#[test]
fn synchronize_identity_quiescence_assertion_is_prominent_in_preview() {
    let ir = r#"{"ir_version":1,"name":"sync_imported_orders","ops":[
      {"op":"synchronizeIdentity","schema":"public","table":"orders","column":"id",
       "writesQuiesced":"orders_import_window"}
    ]}"#;

    for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
        let out = render_ir_envelope_sql(ir, dialect, &opts()).expect("preview");
        assert!(
            out.contains(RUNTIME_RESOLVED)
                && out.contains("SYNCHRONIZE IDENTITY")
                && out.contains("WRITES QUIESCED ASSERTION")
                && out.contains("orders_import_window")
                && out.contains("engine cannot prove writer quiescence")
                && out.contains("never backward"),
            "identity synchronization coordination must be operator-prominent for {dialect:?}:\n{out}"
        );
    }
}

/// NO FABRICATION — a guarded (`ifNotExists`) addColumn carries the runtime catalog-
/// probe label AND its bare DDL has NO invented `IF NOT EXISTS` clause (the guard is
/// a runtime probe, not a native clause).
#[test]
fn guarded_op_labeled_and_bare_ddl_has_no_fabricated_clause() {
    let ir = r#"{"ir_version":1,"name":"g","ops":[
      {"op":"addColumn","table":"codes","column":"flag","type":"boolean","nullable":true,"existenceGuard":"ifNotExists"}
    ]}"#;
    let out = render_ir_envelope_sql(ir, SqlDialect::Postgres, &opts()).expect("renders offline");
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

/// RENDER SUCCEEDS WITHOUT A DSN (truth-in-advertising). Scrubbing
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
    let representative = resolve_envelope_json(REPRESENTATIVE_IR);
    let representative_mysql = resolve_envelope_json(REPRESENTATIVE_IR_MYSQL);
    let pg = render_ir_envelope_sql(&representative, SqlDialect::Postgres, &opts());
    let sqlite = render_ir_envelope_sql(&representative, SqlDialect::Sqlite, &opts());
    let mysql = render_ir_envelope_sql(&representative_mysql, SqlDialect::Mysql, &opts());
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
    let envelope_json = r#"{
      "ir_version": 1,
      "name": "single",
      "ops": [
        {"op":"createTable","name":"widgets","columns":[
          {"name":"sku","type":"text","nullable":false,"unique":true}
        ]}
      ]
    }"#;
    let ir: MigrationIr = serde_json::from_str(envelope_json).unwrap();
    let author = IrAuthor::new(
        "public",
        "app_preview",
        SqlDialect::Postgres,
        &support::no_inject("public"),
    );
    let plan = author
        .lower_plan(&ir, &LiveSchema::default())
        .expect("DB-independent IR lowers offline");

    let out = render_plan_sql(&plan, SqlDialect::Postgres, &opts());
    assert!(
        out.contains("-- plan"),
        "carries the per-plan header:\n{out}"
    );
    assert!(
        out.contains("(dialect: postgres)"),
        "labels the dialect:\n{out}"
    );

    // Every lowered DDL body appears verbatim — faithfulness to the engine's lowering.
    let steps = author.lower_steps(&ir, &LiveSchema::default()).unwrap();
    for step in &steps {
        if let PlanStep::Ddl(m) = step {
            let body = m.up.trim_end().trim_end_matches(';');
            assert!(
                out.contains(body),
                "render_plan_sql missing lowered DDL:\n{body}\n--\n{out}"
            );
        }
    }
}

/// `render_plan_sql` over a PG `OnlineRename(PgExpandContract)` plan. This is
/// the public-API entrypoint for a hand-built rename plan (no CLI path feeds an
/// `OnlineRename` step). It locks the no-fabrication contract for the rename render
/// surface: the expand/contract ADDITIVE DDL must appear ONLY as `--`-comment lines
/// under a `-- [runtime-resolved]` label, and NO bare executable rename SQL (no
/// `ALTER … RENAME`, no `CREATE TRIGGER`, no uncommented expand DDL) may leak.
#[test]
fn render_plan_sql_online_rename_is_labeled_never_fabricated() {
    use zero_migrate::{ExpandContractAuthor, OnlineIntent};
    use zero_migrate::{PlanStep, RenameStep};

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

    // Build a real, fully-formed AppliedPlan in-memory (lower a trivial createTable IR
    // via the same author the engine uses), then swap its single DDL step for the
    // OnlineRename step (the only piece under test).
    let seed_json = r#"{
      "ir_version": 1,
      "name": "seed",
      "ops": [
        {"op":"createTable","name":"codes","columns":[
          {"name":"id","type":"text","nullable":false,"unique":true}
        ]}
      ]
    }"#;
    let seed: MigrationIr = serde_json::from_str(seed_json).unwrap();
    let author = IrAuthor::new(
        "public",
        "app_preview",
        SqlDialect::Postgres,
        &support::no_inject("public"),
    );
    let mut plan = author
        .lower_plan(&seed, &LiveSchema::default())
        .expect("DB-independent IR lowers offline");
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
    assert!(
        out.contains("ADD COLUMN"),
        "the additive expand DDL should be surfaced:\n{out}"
    );

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
    for needle in [
        "ADD COLUMN",
        "CREATE OR REPLACE FUNCTION",
        "CREATE",
        "TRIGGER",
    ] {
        for line in out.lines().filter(|l| l.contains(needle)) {
            assert!(
                line.trim_start().starts_with("--"),
                "rename mechanic {needle:?} must be a comment, not executable: {line:?}\n{out}"
            );
        }
    }
}

/// A malformed IR envelope is a hard error (the CLI maps this to a non-zero exit).
#[test]
fn malformed_ir_is_error() {
    let err = render_ir_envelope_sql("{ not json", SqlDialect::Postgres, &opts());
    assert!(err.is_err(), "malformed IR must be an error");
}

// NOTE: the three offline `plan` CLI-smoke tests that shelled
// the retired Rust `zero-migrate` binary (`CARGO_BIN_EXE_zero-migrate`) were removed
// with the bin. The offline SQL-preview surface they exercised — `render_ir_envelope_sql`
// / `render_set_sql` / `render_plan_sql` + the `-- [runtime-resolved]` labeling — is
// still fully covered DB-free by the library tests above (goldens, faithfulness,
// no-fabrication, `render_succeeds_without_a_dsn`). The command-line entry point is
// now the `zero-migrate-cli` TS CLI (`packages/zero-migrate-cli/src/cli.ts`).

/// `render_set_sql` — the multi-plan renderer. Lower a DB-independent IR to an
/// `AppliedPlan` in-memory and render it as a one-element set, asserting the summary
/// line + the lowered DDL surface. Symmetric with the `render_plan_sql` test above.
#[test]
fn render_set_sql_surfaces_lowered_ddl_offline() {
    let envelope_json = r#"{
      "ir_version": 1,
      "name": "widgets",
      "ops": [
        {"op":"createTable","name":"widgets","columns":[
          {"name":"id","type":"text","nullable":false,"unique":true}
        ]}
      ]
    }"#;
    let ir: MigrationIr = serde_json::from_str(envelope_json).unwrap();
    let author = IrAuthor::new(
        "public",
        "app_preview",
        SqlDialect::Postgres,
        &support::no_inject("public"),
    );
    let plan = author
        .lower_plan(&ir, &LiveSchema::default())
        .expect("DB-independent IR lowers offline");

    let out = render_set_sql(&[plan], SqlDialect::Postgres, &opts());
    assert!(
        out.contains("CREATE TABLE"),
        "the lowered DDL should surface:\n{out}"
    );
    assert!(
        out.contains("-- preview:"),
        "carries a summary line:\n{out}"
    );
}

/// A bounded `t.string({ length })` column (`ColType::String { length }`) renders a
/// portable `VARCHAR(N)` on Postgres/MySQL and `TEXT` on SQLite — the "write once"
/// bounded-string contract. `t.text()` (`ColType::Text`) stays unbounded.
#[test]
fn string_length_renders_bounded_varchar_across_dialects() {
    let ir = r#"{"ir_version":1,"name":"bounded","ops":[
      {"op":"createTable","name":"widgets","columns":[
        {"name":"id","type":"int","nullable":false,"identity":{"always":false}},
        {"name":"code","type":{"string":{"length":200}},"nullable":false}
      ],"primaryKey":["id"]}
    ]}"#;
    let resolved = resolve_envelope_json(ir);

    let pg = render_ir_envelope_sql(&resolved, SqlDialect::Postgres, &opts())
        .expect("bounded string renders on PG");
    assert!(
        pg.contains("character varying(200)") || pg.contains("varchar(200)"),
        "PG must render a bounded varchar(200):\n{pg}"
    );

    let mysql = render_ir_envelope_sql(&resolved, SqlDialect::Mysql, &opts())
        .expect("bounded string renders on MySQL");
    assert!(
        mysql.contains("VARCHAR(200)"),
        "MySQL must render VARCHAR(200), not the legacy VARCHAR(191) cap:\n{mysql}"
    );

    let sqlite = render_ir_envelope_sql(&resolved, SqlDialect::Sqlite, &opts())
        .expect("bounded string renders on SQLite");
    assert!(
        sqlite.to_uppercase().contains("\"CODE\" TEXT")
            || sqlite.contains("code") && sqlite.contains("TEXT"),
        "SQLite stores the bounded string as TEXT:\n{sqlite}"
    );
}

/// A constraint body is built once in PostgreSQL spelling and re-quoted for MySQL.
/// That rewrite must not let an identifier become SQL structure: a column carrying a
/// backtick used to close the identifier early and contribute an index the author
/// never declared, and one carrying an apostrophe used to swallow the closing quote.
#[test]
fn mysql_constraint_requoting_escapes_hostile_identifiers() {
    let ir = r#"{
  "ir_version": 1,
  "name": "hostile",
  "ops": [
    {"op":"createTable","name":"t","columns":[
      {"name":"COLNAME","type":{"string":{"length":64}}}
    ],"constraints":[
      {"name":"u1","kind":{"kind":"unique","columns":["COLNAME"]}}
    ]}
  ]
}"#;
    let render = |col: &str| -> String {
        let raw: MigrationIr =
            serde_json::from_str(&ir.replace("COLNAME", col)).expect("hostile IR parses");
        let resolved = resolve_create_table_policy(&raw, &support::confined_charter(), "public")
            .expect("hostile IR resolves");
        let json = serde_json::to_string(&resolved).expect("resolved IR serializes");
        render_ir_envelope_sql(&json, SqlDialect::Mysql, &opts())
            .expect("hostile IR renders offline")
            .lines()
            .find(|l| l.starts_with("CREATE TABLE"))
            .expect("a CREATE TABLE statement")
            .to_string()
    };

    // An apostrophe is an ordinary character inside a quoted identifier, so the
    // identifier must still close on its own delimiter.
    let apostrophe = render("it's");
    assert!(
        apostrophe.contains("UNIQUE (`it's`)"),
        "apostrophe column must close its identifier: {apostrophe}"
    );
    assert!(
        !apostrophe.contains('"'),
        "no PostgreSQL quoting may survive into MySQL DDL: {apostrophe}"
    );

    // A backtick is the MySQL delimiter, so it must be doubled rather than emitted
    // raw. Undoubled, `a`), KEY `k2` (`id` closes the identifier and injects an index.
    let backtick = render("a`), KEY `k2` (`id");
    assert!(
        backtick.contains("UNIQUE (`a``), KEY ``k2`` (``id`)"),
        "backtick column must be doubled inside the constraint: {backtick}"
    );
    assert_eq!(
        backtick.matches("KEY ``k2``").count(),
        backtick.matches("k2").count(),
        "every occurrence of the authored name stays escaped, so none of it becomes \
         a KEY clause: {backtick}"
    );
}
