//! Live-PG coverage for the IR-document apply core used by deploy plumbing.
//!
//! Model C removed the Platform runner's physical IR corpus branch: platform
//! migrations are `.ts`-only and record transient IR at migrate time. The shared
//! IR-document apply core remains for creator/control-plane deploys, so this file
//! keeps the Confined denial coverage and pins the runner's Platform IR refusal.

use std::path::{Path, PathBuf};

use compio_postgres::Client;
use zeroship_migrate::command::runner::{run_migrate, RunConfig, RunProfile};
use zeroship_migrate::test_support::acquire_global_platform_resource_lock;
use zeroship_migrate::{Approval, ExecutorConfig, GuardConfig, PolicyProfile, PostgresBackend};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_ir_test";
const TEST_DB_NAME: &str = "zeroship_migrate_ir_test";

const CONFINED_ROLE: &str = "zeroship_ir_confined_role";
const PLATFORM_VENDOR_IR: &str = r#"{
  "ir_version": 6,
  "name": "platform_vendor_ir",
  "owner_app": "platform",
  "ops": [
    {
      "op": "createExtension",
      "name": "citext",
      "ifNotExists": true
    },
    {
      "op": "createSchema",
      "name": "zeroship",
      "ifNotExists": true
    },
    {
      "op": "createRole",
      "name": "zeroship_ir_test_app",
      "login": true,
      "password": "zeroship_ir_test_app",
      "setSearchPath": [
        "zeroship",
        "public"
      ],
      "ifNotExists": true
    },
    {
      "op": "createTable",
      "name": "ir_accounts",
      "schema": "zeroship",
      "columns": [
        {
          "name": "id",
          "type": "int",
          "nullable": false,
          "identity": {
            "always": true
          }
        },
        {
          "name": "app_id",
          "type": "text",
          "nullable": false
        },
        {
          "name": "email",
          "type": "text",
          "nullable": false
        }
      ],
      "constraints": [],
      "indexes": []
    },
    {
      "op": "grant",
      "privileges": [
        "select",
        "insert",
        "update",
        "delete"
      ],
      "on": {
        "kind": "table",
        "names": [
          "ir_accounts"
        ],
        "schema": "zeroship"
      },
      "to": [
        "zeroship_ir_test_app"
      ]
    },
    {
      "op": "enableRls",
      "table": "ir_accounts",
      "schema": "zeroship"
    },
    {
      "op": "forceRls",
      "table": "ir_accounts",
      "schema": "zeroship"
    },
    {
      "op": "createPolicy",
      "name": "tenant_isolation",
      "table": "ir_accounts",
      "schema": "zeroship",
      "forCmd": "all",
      "using": {
        "node": "binOp",
        "op": "eq",
        "lhs": {
          "node": "colRef",
          "name": "app_id"
        },
        "rhs": {
          "node": "cast",
          "operand": {
            "node": "fnCall",
            "fn": "currentSetting",
            "args": [
              {
                "node": "literal",
                "value": "zeroship.tenant_app"
              },
              {
                "node": "literal",
                "value": true
              }
            ]
          },
          "target": "text"
        }
      },
      "withCheck": {
        "node": "binOp",
        "op": "eq",
        "lhs": {
          "node": "colRef",
          "name": "app_id"
        },
        "rhs": {
          "node": "cast",
          "operand": {
            "node": "fnCall",
            "fn": "currentSetting",
            "args": [
              {
                "node": "literal",
                "value": "zeroship.tenant_app"
              },
              {
                "node": "literal",
                "value": true
              }
            ]
          },
          "target": "text"
        }
      }
    }
  ]
}
"#;
const CONFINED_ROLE_IR: &str = r#"{
  "ir_version": 6,
  "name": "confined_role_denied",
  "owner_app": "app_confined",
  "ops": [
    {
      "op": "createRole",
      "name": "zeroship_ir_confined_role",
      "ifNotExists": true
    }
  ]
}
"#;
const CONFINED_GRANT_IR: &str = r#"{
  "ir_version": 6,
  "name": "confined_grant_denied",
  "owner_app": "app_confined",
  "ops": [
    {
      "op": "createTable",
      "name": "ir_confined_grants",
      "schema": "zeroship",
      "columns": [
        {
          "name": "id",
          "type": "text",
          "nullable": false
        }
      ],
      "constraints": [],
      "indexes": []
    },
    {
      "op": "grant",
      "privileges": [
        "select"
      ],
      "on": {
        "kind": "table",
        "names": [
          "ir_confined_grants"
        ],
        "schema": "zeroship"
      },
      "to": [
        "public"
      ]
    }
  ]
}
"#;
const PLATFORM_ATTACH_TS: &str = r#"
import { table, t } from "@zeroship/migrate";
import { createFunction, schema } from "@zeroship/migrate/pg";

export const name = "platform_attach";

export function up() {
  schema({ name: "zeroship", ifNotExists: true });

  table("platform_apps", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });

  table("platform_registry", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      route: t.text().notNull(),
      target: t.text().notNull(),
      status: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id", "route"],
    checks: [
      { name: "platform_registry_target_nonempty", expr: (c) => c("target").ne("") },
    ],
  });

  const registry = table("platform_registry", { schema: "zeroship" });
  registry.foreignKey("platform_registry_app_fk").add({
    columns: ["app_id"],
    references: { table: "platform_apps", columns: ["id"] },
  });
  registry.check("platform_registry_status_check").add({
    expr: (c) => c.pg.eqAnyArray(c("status"), ["active", "paused"]),
  });
  registry.index("platform_registry_target_idx").add({ columns: ["target"] });
  registry.enableRowLevelSecurity();
  registry.forceRowLevelSecurity();
  registry.createPolicy({
    name: "tenant_isolation",
    for: "all",
    using: (c) => c("app_id").isNotNull(),
    withCheck: (c) => c("app_id").isNotNull(),
  });
  registry.comment("Platform route registry");

  createFunction({
    name: "platform_registry_touch",
    schema: "zeroship",
    returns: "trigger",
    language: "plpgsql",
    replace: true,
    body: "BEGIN RETURN NEW; END;",
  });
  registry.createTrigger({
    name: "platform_registry_touch_trg",
    timing: "before",
    events: ["update"],
    forEach: "row",
    execute: "platform_registry_touch",
  });
}
"#;
const PLATFORM_SYNTH_DEFAULT_TS: &str = r#"
import { table, t } from "@zeroship/migrate";
import { schema } from "@zeroship/migrate/pg";

export const name = "platform_synth_defaults";

export function up() {
  schema({ name: "zeroship", ifNotExists: true });

  table("platform_events", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      occurred_at: t.timestamp().notNull().default({ fn: "now" }),
      kind: t.text().notNull(),
    },
    primaryKey: ["id"],
  });
}
"#;

fn dsn() -> String {
    std::env::var("MIGRATE_PLATFORM_IR_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

fn maintenance_dsn() -> String {
    dsn()
        .split_whitespace()
        .map(|tok| {
            if tok.starts_with("dbname=") {
                "dbname=postgres".to_string()
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn ensure_dedicated_db() {
    let raw = dsn();
    if !raw.contains(&format!("dbname={TEST_DB_NAME}")) {
        return;
    }
    let (client, conn) = compio_postgres::connect(&maintenance_dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to maintenance postgres DB on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    let exists = !client
        .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&TEST_DB_NAME])
        .await
        .expect("query pg_database")
        .is_empty();
    if !exists {
        if let Err(e) = client
            .batch_execute(&format!(r#"CREATE DATABASE "{TEST_DB_NAME}""#))
            .await
        {
            let exists_after_race = !client
                .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&TEST_DB_NAME])
                .await
                .expect("query pg_database after create failure")
                .is_empty();
            assert!(
                exists_after_race,
                "create dedicated platform IR test database failed and DB is still absent: {e}"
            );
        }
    }
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_ir_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

fn platform_cfg(dir: &Path, meta: &str, yes: bool) -> RunConfig {
    RunConfig {
        dir: dir.to_path_buf(),
        database_url: dsn(),
        engine_override: None,
        profile: RunProfile::Platform,
        project_id: "platform-ir-test".to_string(),
        project_schema: "zeroship".to_string(),
        schemas: vec!["zeroship".to_string(), "public".to_string()],
        extensions: vec!["citext".to_string()],
        meta_schema: meta.to_string(),
        yes,
        statement_timeout: std::time::Duration::from_secs(60),
        lock_timeout: std::time::Duration::from_secs(30),
    }
}

fn confined_exec_cfg(meta: &str) -> ExecutorConfig {
    let mut cfg = ExecutorConfig::new("confined-ir-test", "zeroship");
    cfg.pg.meta_schema = meta.to_string();
    cfg
}

async fn reset(conn: &Client, meta: &str) {
    conn.batch_execute(
        r#"DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_ir_confined_role') THEN
    DROP OWNED BY "zeroship_ir_confined_role";
  END IF;
END $$;"#,
    )
    .await
    .expect("drop owned by test roles");
    conn.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS zeroship CASCADE; \
         DROP SCHEMA IF EXISTS \"{meta}\" CASCADE; \
         DROP EXTENSION IF EXISTS citext CASCADE;"
    ))
    .await
    .expect("reset schemas/extensions");
    conn.batch_execute(&format!("DROP ROLE IF EXISTS \"{CONFINED_ROLE}\";"))
        .await
        .expect("drop test roles");
}

async fn role_exists(conn: &Client, role: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .expect("query pg_roles")
        .is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query information_schema.tables")
        .is_empty()
}

async fn primary_key_columns(conn: &Client, schema: &str, table: &str) -> Vec<String> {
    conn.query(
        "SELECT a.attname \
         FROM pg_constraint c \
         JOIN pg_class t ON t.oid = c.conrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true \
         JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
         WHERE n.nspname = $1 AND t.relname = $2 AND c.contype = 'p' \
         ORDER BY k.ord",
        &[&schema, &table],
    )
    .await
    .expect("query primary key columns")
    .into_iter()
    .map(|row| row.get::<_, String>(0))
    .collect()
}

async fn table_columns(conn: &Client, schema: &str, table: &str) -> Vec<String> {
    conn.query(
        "SELECT column_name \
         FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = $2 \
         ORDER BY ordinal_position",
        &[&schema, &table],
    )
    .await
    .expect("query table columns")
    .into_iter()
    .map(|row| row.get::<_, String>(0))
    .collect()
}

async fn column_udt_name(
    conn: &Client,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<String> {
    let rows = conn
        .query(
            "SELECT udt_name \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column type");
    rows.first().map(|row| row.get::<_, String>(0))
}

fn sorted_columns(mut cols: Vec<String>) -> Vec<String> {
    cols.sort();
    cols
}

async fn column_default_expr(
    conn: &Client,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<String> {
    let rows = conn
        .query(
            "SELECT pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_attribute a \
             JOIN pg_class t ON t.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE n.nspname = $1 AND t.relname = $2 AND a.attname = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column default");
    rows.first().map(|row| row.get::<_, String>(0))
}

async fn relation_rls(conn: &Client, schema: &str, table: &str) -> (bool, bool) {
    let rows = conn
        .query(
            "SELECT c.relrowsecurity, c.relforcerowsecurity \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &table],
        )
        .await
        .expect("query relation RLS flags");
    let row = rows.first().expect("relation exists");
    (row.get::<_, bool>(0), row.get::<_, bool>(1))
}

async fn policy_exists(conn: &Client, schema: &str, table: &str, policy: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM pg_policies \
             WHERE schemaname = $1 AND tablename = $2 AND policyname = $3",
            &[&schema, &table, &policy],
        )
        .await
        .expect("query pg_policies")
        .is_empty()
}

async fn table_comment(conn: &Client, schema: &str, table: &str) -> Option<String> {
    let rows = conn
        .query(
            "SELECT obj_description(c.oid, 'pg_class') \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table comment");
    rows.first().and_then(|row| row.get::<_, Option<String>>(0))
}

async fn index_exists(conn: &Client, schema: &str, table: &str, index: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM pg_indexes \
             WHERE schemaname = $1 AND tablename = $2 AND indexname = $3",
            &[&schema, &table, &index],
        )
        .await
        .expect("query pg_indexes")
        .is_empty()
}

async fn constraint_kind(
    conn: &Client,
    schema: &str,
    table: &str,
    constraint: &str,
) -> Option<String> {
    let rows = conn
        .query(
            "SELECT c.contype::text \
             FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = $1 AND t.relname = $2 AND c.conname = $3",
            &[&schema, &table, &constraint],
        )
        .await
        .expect("query pg_constraint");
    rows.first().map(|row| row.get::<_, String>(0))
}

async fn trigger_exists(conn: &Client, schema: &str, table: &str, trigger: &str) -> bool {
    !conn
        .query(
            "SELECT 1 \
             FROM pg_trigger tr \
             JOIN pg_class t ON t.oid = tr.tgrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = $1 AND t.relname = $2 AND tr.tgname = $3 \
               AND NOT tr.tgisinternal",
            &[&schema, &table, &trigger],
        )
        .await
        .expect("query pg_trigger")
        .is_empty()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tok: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("zsmig_platform_ir_{tok}"));
        std::fs::create_dir_all(&dir).expect("create temp migration dir");
        Self(dir)
    }

    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.0.join(name), body).expect("write migration file");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn transient_ir_corpus(tok: &str, name: &str, file_name: &str, body: &str) -> TempDir {
    let dir = TempDir::new(&format!("{tok}_{name}"));
    dir.write(file_name, body);
    dir
}

#[compio::test]
async fn platform_runner_rejects_physical_ir_corpus() {
    let tok = token();
    let dir = transient_ir_corpus(
        &tok,
        "apply",
        "20260630000000_platform_vendor.ir.json",
        PLATFORM_VENDOR_IR,
    );
    let cfg = platform_cfg(dir.path(), "platform_ir_meta_unused", false);
    let err = run_migrate(&cfg)
        .await
        .expect_err("Platform runner no longer accepts physical IR corpora");
    assert!(
        format!("{err}").contains("unsupported platform migration corpus"),
        "got: {err}"
    );
}

#[compio::test]
async fn confined_ir_still_denies_role_and_grant_vendor_ops() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let backend = PostgresBackend::new(&conn);
    let guard = GuardConfig::confined("zeroship");
    let tok = token();

    let role_meta = format!("confined_ir_role_meta_{}", token());
    reset(&conn, &role_meta).await;
    let role_cfg = confined_exec_cfg(&role_meta);
    let role_dir = transient_ir_corpus(
        &tok,
        "confined_role",
        "20260630000000_role.ir.json",
        CONFINED_ROLE_IR,
    );
    let role_err = zeroship_migrate::apply_bundle_ir_postgres(
        &backend,
        "zeroship",
        "app_confined",
        role_dir.path(),
        &role_cfg,
        &guard,
        &PolicyProfile::confined(),
        Approval::Approved,
        "confined-ir-test",
    )
    .await
    .expect_err("Confined IR must reject createRole");
    let role_msg = format!("{role_err}").to_ascii_lowercase();
    assert!(
        role_msg.contains("role") || role_msg.contains("vendor_op_denied"),
        "role denial should identify the denied role primitive, got: {role_err}"
    );
    assert!(
        !role_exists(&conn, CONFINED_ROLE).await,
        "denied createRole did not materialize"
    );

    let grant_meta = format!("confined_ir_grant_meta_{}", token());
    reset(&conn, &grant_meta).await;
    let grant_cfg = confined_exec_cfg(&grant_meta);
    let grant_dir = transient_ir_corpus(
        &tok,
        "confined_grant",
        "20260630000000_grant.ir.json",
        CONFINED_GRANT_IR,
    );
    let grant_err = zeroship_migrate::apply_bundle_ir_postgres(
        &backend,
        "zeroship",
        "app_confined",
        grant_dir.path(),
        &grant_cfg,
        &guard,
        &PolicyProfile::confined(),
        Approval::Approved,
        "confined-ir-test",
    )
    .await
    .expect_err("Confined IR must reject grant");
    let grant_msg = format!("{grant_err}").to_ascii_lowercase();
    assert!(
        grant_msg.contains("grant") || grant_msg.contains("vendor_op_denied"),
        "grant denial should identify grant, got: {grant_err}"
    );
    assert!(
        !table_exists(&conn, "zeroship", "ir_confined_grants").await,
        "guard-per-fragment denial happens before the createTable applies"
    );

    reset(&conn, &grant_meta).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_author_synth_defaults_render_and_apply_on_live_pg() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_synth_meta_{tok}");
    reset(&conn, &meta).await;

    let dir = transient_ir_corpus(
        &tok,
        "platform_synth_defaults",
        "20260703000000_platform_synth_defaults.ts",
        PLATFORM_SYNTH_DEFAULT_TS,
    );
    let cfg = platform_cfg(dir.path(), &meta, true);
    run_migrate(&cfg)
        .await
        .expect("Platform TS migration with author synth defaults applies");

    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_events", "id")
            .await
            .as_deref(),
        Some("uuid"),
        "author t.uuid() column renders as uuid"
    );
    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_events", "occurred_at")
            .await
            .as_deref(),
        Some("timestamptz"),
        "author t.timestamp() column renders as timestamptz"
    );
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_events", "id")
            .await
            .as_deref(),
        Some("gen_random_uuid()"),
        "author uuid synth default rendered as DEFAULT gen_random_uuid()"
    );
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_events", "occurred_at")
            .await
            .as_deref(),
        Some("now()"),
        "author timestamp synth default rendered as DEFAULT now()"
    );
    conn.batch_execute(
        "INSERT INTO zeroship.platform_events (kind) VALUES ('boot'); \
         SELECT id, occurred_at FROM zeroship.platform_events WHERE kind = 'boot';",
    )
    .await
    .expect("insert using both synth defaults");

    reset(&conn, &meta).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_ts_exact_create_table_structural_attachments_apply_on_live_pg() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_attach_meta_{tok}");
    reset(&conn, &meta).await;

    let dir = transient_ir_corpus(
        &tok,
        "platform_attach",
        "20260702000000_platform_attach.ts",
        PLATFORM_ATTACH_TS,
    );
    let cfg = platform_cfg(dir.path(), &meta, true);
    run_migrate(&cfg)
        .await
        .expect("Platform TS migration with structural attachments applies");

    assert!(
        table_exists(&conn, "zeroship", "platform_registry").await,
        "platform-exact table materialized"
    );
    assert_eq!(
        sorted_columns(table_columns(&conn, "zeroship", "platform_apps").await),
        vec!["created_at".to_string(), "id".to_string()],
        "platform CreateTable materializes exactly the author columns on the FK target"
    );
    assert_eq!(
        sorted_columns(table_columns(&conn, "zeroship", "platform_registry").await),
        vec![
            "app_id".to_string(),
            "created_at".to_string(),
            "route".to_string(),
            "status".to_string(),
            "target".to_string(),
        ],
        "platform CreateTable materializes exactly the author columns, with no confined system fields"
    );
    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_apps", "id")
            .await
            .as_deref(),
        Some("uuid"),
        "platform FK target id renders as uuid"
    );
    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_registry", "app_id")
            .await
            .as_deref(),
        Some("uuid"),
        "platform FK column renders as uuid"
    );
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_apps", "id")
            .await
            .as_deref(),
        Some("gen_random_uuid()"),
        "guard target table carries author genRandomUuid default"
    );
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_registry", "created_at")
            .await
            .as_deref(),
        Some("now()"),
        "guard registry table carries author now default"
    );
    assert_eq!(
        primary_key_columns(&conn, "zeroship", "platform_registry").await,
        vec!["app_id".to_string(), "route".to_string()],
        "platform CreateTable keeps the author composite primary key"
    );
    assert_eq!(
        constraint_kind(
            &conn,
            "zeroship",
            "platform_registry",
            "platform_registry_app_fk",
        )
        .await
        .as_deref(),
        Some("f"),
        "same-file FK attach materialized"
    );
    assert_eq!(
        constraint_kind(
            &conn,
            "zeroship",
            "platform_registry",
            "platform_registry_target_nonempty",
        )
        .await
        .as_deref(),
        Some("c"),
        "table-level CHECK from createTable materialized"
    );
    assert_eq!(
        constraint_kind(
            &conn,
            "zeroship",
            "platform_registry",
            "platform_registry_status_check",
        )
        .await
        .as_deref(),
        Some("c"),
        "PG membership CHECK attach materialized"
    );
    assert!(
        index_exists(
            &conn,
            "zeroship",
            "platform_registry",
            "platform_registry_target_idx",
        )
        .await,
        "same-file index attach materialized"
    );
    assert_eq!(
        relation_rls(&conn, "zeroship", "platform_registry").await,
        (true, true),
        "enableRls + forceRls attached to the platform-exact table"
    );
    assert!(
        policy_exists(&conn, "zeroship", "platform_registry", "tenant_isolation").await,
        "createPolicy attached to the platform-exact table"
    );
    assert_eq!(
        table_comment(&conn, "zeroship", "platform_registry").await.as_deref(),
        Some("Platform route registry"),
        "comment attached to the platform-exact table"
    );
    assert!(
        trigger_exists(
            &conn,
            "zeroship",
            "platform_registry",
            "platform_registry_touch_trg",
        )
        .await,
        "createTrigger attached to the platform-exact table"
    );

    reset(&conn, &meta).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_runner_rejects_mixed_ts_ir_and_sql_corpus_before_connect() {
    let tok = token();
    let dir = TempDir::new(&tok);
    dir.write("20260630000000_mixed.ts", "export function up() {}\n");
    dir.write("20260630000000_mixed.ir.json", "{}");
    dir.write("V0001__mixed.sql", "SELECT 1;");

    let cfg = platform_cfg(dir.path(), "mixed_meta", false);
    let err = run_migrate(&cfg)
        .await
        .expect_err("mixed TS/IR/SQL corpus must be rejected before loader/apply");
    assert!(
        format!("{err}").contains("mixed platform migration corpus"),
        "got: {err}"
    );
}
