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
      "op": "setRls",
      "table": "ir_accounts",
      "schema": "zeroship",
      "enabled": true,
      "forced": true
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
import { table, t, now, genRandomUuid } from "@zeroship/migrate";
import { createFunction, pgTable, schema } from "@zeroship/migrate/pg";

export const name = "platform_attach";

export function up() {
  schema("zeroship").create({ ifNotExists: true });

  table("platform_apps", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });

  table("platform_registry", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      route: t.text().notNull(),
      target: t.text().notNull(),
      status: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["app_id", "route"],
    checks: [
      { name: "platform_registry_target_nonempty", expr: (col) => col("target").ne("") },
    ],
  });

  const registry = pgTable("platform_registry", { schema: "zeroship" });
  registry.foreignKey("platform_registry_app_fk").add({
    columns: ["app_id"],
    references: { table: "platform_apps", columns: ["id"] },
  });
  registry.check("platform_registry_status_check").add({
    expr: (col) => col("status").in(["active", "paused"]),
  });
  registry.index("platform_registry_target_idx").add({ on: ["target"] });
  registry.setRls({ enabled: true, forced: true });
  registry.policy("tenant_isolation").create({
    for: "all",
    using: (col) => col("app_id").isNotNull(),
    withCheck: (col) => col("app_id").isNotNull(),
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
  registry.trigger("platform_registry_touch_trg").create({
    timing: "before",
    events: ["update"],
    forEach: "row",
    execute: "platform_registry_touch",
  });
}
"#;
const PLATFORM_COMPOSITE_FK_TS: &str = r#"
import { table, t } from "@zeroship/migrate";
import { pgTable, schema } from "@zeroship/migrate/pg";

export const name = "platform_composite_fk";

export function up() {
  schema("zeroship").create({ ifNotExists: true });

  table("oauth_clients", { schema: "zeroship" }).create({
    columns: {
      client_id: t.text().notNull(),
    },
    primaryKey: ["client_id"],
  });
  table("app_oauth_clients", { schema: "zeroship" }).create({
    columns: {
      client_id: t.text().notNull(),
    },
    primaryKey: null,
  });
  table("app_oauth_clients", { schema: "zeroship" }).foreignKey("app_oauth_clients_client_id_fkey").add({
    columns: ["client_id"],
    references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" },
    onDelete: "cascade",
  });

  table("invoice_lines", { schema: "zeroship" }).create({
    columns: {
      invoice_id: t.uuid().notNull(),
      app_id: t.uuid().notNull(),
      segment_no: t.int().notNull(),
    },
    primaryKey: ["invoice_id", "app_id", "segment_no"],
  });
  table("billing_line_provider_refs", { schema: "zeroship" }).create({
    columns: {
      invoice_id: t.uuid().notNull(),
      app_id: t.uuid().notNull(),
      segment_no: t.int().notNull(),
    },
    primaryKey: null,
  });
  table("billing_line_provider_refs", { schema: "zeroship" }).foreignKey("billing_line_provider_refs_line_fk").add({
    columns: ["invoice_id", "app_id", "segment_no"],
    references: {
      table: "invoice_lines",
      columns: ["invoice_id", "app_id", "segment_no"],
      schema: "zeroship",
    },
    onDelete: "cascade",
  });
}
"#;
const PLATFORM_SYNTH_DEFAULT_TS: &str = r#"
import { table, t, now, genRandomUuid } from "@zeroship/migrate";
import { schema } from "@zeroship/migrate/pg";

export const name = "platform_synth_defaults";

export function up() {
  schema("zeroship").create({ ifNotExists: true });

  table("platform_events", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      occurred_at: t.timestamp().notNull().default(now()),
      kind: t.text().notNull(),
      payload: t.json().notNull(),
      items: t.json().notNull(),
      settings: t.json().notNull().default("seed"),
    },
    primaryKey: ["id"],
  });
}
"#;
const PLATFORM_EXPR_SURFACE_TS: &str = r#"
import {
  table,
  t,
  check,
  interval,
} from "@zeroship/migrate";
import { pgTable, schema } from "@zeroship/migrate/pg";

export const name = "platform_expr_surface";

export function up() {
  schema("zeroship").create({ ifNotExists: true });

  pgTable("expr_surface", { schema: "zeroship" }).create({
    columns: {
      pkce_method: t.text().notNull(),
      amount_cents: t.int().notNull(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      data: t.json().notNull(),
      subtotal_cents: t.int().notNull(),
      credit_cents: t.int().notNull(),
      total_cents: t.int().notNull(),
      floor_cents: t.int(),
      created_at: t.timestamp().notNull(),
      expires_at: t.timestamp().notNull(),
      active: t.boolean().notNull(),
      visible: t.boolean().notNull(),
      status: t.text().notNull(),
      snapshot_artifact_path: t.text(),
      snapshot_sha256: t.text(),
      snapshot_ch_version: t.text(),
    },
    checks: [
      check("expr_pkce_method_check", (col) => col("pkce_method").eq("S256")),
      check("expr_user_id_fmt", (col) => col("user_id").regex("^usr_[0-9A-Za-z]{20,40}$")),
      check("expr_kind_ok", (col) => col("kind").in(["a", "b", "c"])),
      check("expr_kind_not_reserved", (col) => col("kind").notIn(["x", "y"])),
      check("expr_data_size", (col) => col("data").columnSize().lt(262144)),
      check("expr_total_matches", (col) => col("total_cents").eq(col("subtotal_cents").sub(col("credit_cents")))),
      check("expr_floor_nonneg_or_null", (col) => col("floor_cents").isNull().or(col("floor_cents").ge(0))),
      check("expr_active_visible", (col) => col("active").and(col("visible"))),
      { name: "expr_expires_window", expr: (col) => col("expires_at").le(col("created_at").add(interval({ minutes: 1 }))) },
      // Mirrors the platform sandboxes_snapshot_artifact_consistency marker:
      // a <> ALL negated inList OR'd with a 3-way IS NOT NULL AND chain.
      check("expr_snapshot_consistency", (col) =>
        col("status").notIn(["snapshotted", "snapshotted_suspect"]).or(
          col("snapshot_artifact_path").isNotNull()
            .and(col("snapshot_sha256").isNotNull())
            .and(col("snapshot_ch_version").isNotNull()),
        )),
    ],
  });

  table("expr_surface", { schema: "zeroship" }).check("expr_amount_nonnegative").add({
    expr: (col) => col("amount_cents").ge(0),
  });

  // Partial index whose predicate is a notIn (<> ALL on PG) — mirrors the
  // platform wake_jobs partial indexes.
  pgTable("expr_surface", { schema: "zeroship" })
    .index("expr_status_partial_idx")
    .add({ on: ["status"], where: (col) => col("status").notIn(["snapshotted", "snapshotted_suspect"]) });

  table("expr_surface", { schema: "zeroship" })
    .index("expr_created_desc_idx")
    .add({ on: [{ column: "created_at", order: "desc" }] });

  table("expr_surface", { schema: "zeroship" })
    .index("expr_user_created_desc_idx")
    .add({ on: ["user_id", { column: "created_at", order: "desc" }] });
}
"#;
const PLATFORM_SCALAR_TYPES_TS: &str = r#"
import { table, t, genRandomUuid } from "@zeroship/migrate";
import { schema } from "@zeroship/migrate/pg";

export const name = "platform_scalar_types";

export function up() {
  schema("zeroship").create({ ifNotExists: true });

  table("platform_scalar_types", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      shard: t.smallInt().notNull(),
      ratio: t.real().notNull(),
      source_ip: t.inet(),
      scopes: t.textArray().notNull(),
      currency: t.char({ length: 3 }).notNull().default("usd"),
    },
    primaryKey: ["id"],
  });
}
"#;
const PLATFORM_DOMAIN_COLUMN_TS: &str = r#"
import { table, t } from "@zeroship/migrate";
import { domain, schema } from "@zeroship/migrate/pg";

export const name = "platform_domain_column";

export function up() {
  schema("zeroship").create({ ifNotExists: true });

  domain("myd").create({
    schema: "zeroship",
    as: t.text(),
    check: (v) => v.in(["a", "b"]),
  });

  table("domain_surface", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      state: t.domain("myd").notNull(),
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

async fn column_information_schema_type(
    conn: &Client,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<(String, String)> {
    let rows = conn
        .query(
            "SELECT data_type, udt_name \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column information_schema type");
    rows.first()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
}

async fn column_domain_information_schema(
    conn: &Client,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<(Option<String>, Option<String>, String)> {
    let rows = conn
        .query(
            "SELECT domain_name, domain_schema, udt_name \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column domain information_schema type");
    rows.first().map(|row| {
        (
            row.get::<_, Option<String>>(0),
            row.get::<_, Option<String>>(1),
            row.get::<_, String>(2),
        )
    })
}

async fn column_catalog_type_name(
    conn: &Client,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<(String, String)> {
    let rows = conn
        .query(
            "SELECT tn.nspname, ty.typname \
             FROM pg_attribute a \
             JOIN pg_class t ON t.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             JOIN pg_type ty ON ty.oid = a.atttypid \
             JOIN pg_namespace tn ON tn.oid = ty.typnamespace \
             WHERE n.nspname = $1 AND t.relname = $2 AND a.attname = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column catalog type");
    rows.first()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
}

async fn domain_constraint_definition(
    conn: &Client,
    schema: &str,
    domain: &str,
) -> Option<String> {
    let rows = conn
        .query(
            "SELECT pg_get_constraintdef(c.oid) \
             FROM pg_type ty \
             JOIN pg_namespace n ON n.oid = ty.typnamespace \
             JOIN pg_constraint c ON c.contypid = ty.oid \
             WHERE n.nspname = $1 AND ty.typname = $2 AND c.contype = 'c'",
            &[&schema, &domain],
        )
        .await
        .expect("query domain pg_get_constraintdef");
    rows.first().map(|row| row.get::<_, String>(0))
}

async fn column_character_maximum_length(
    conn: &Client,
    schema: &str,
    table: &str,
    column: &str,
) -> Option<i32> {
    let rows = conn
        .query(
            "SELECT character_maximum_length \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column character length");
    rows.first().and_then(|row| row.get::<_, Option<i32>>(0))
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

async fn constraint_definition(
    conn: &Client,
    schema: &str,
    table: &str,
    constraint: &str,
) -> Option<String> {
    let rows = conn
        .query(
            "SELECT pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = $1 AND t.relname = $2 AND c.conname = $3",
            &[&schema, &table, &constraint],
        )
        .await
        .expect("query pg_get_constraintdef");
    rows.first().map(|row| row.get::<_, String>(0))
}

async fn index_definition(conn: &Client, schema: &str, index: &str) -> Option<String> {
    let rows = conn
        .query(
            "SELECT pg_get_indexdef(i.indexrelid) \
             FROM pg_index i \
             JOIN pg_class ic ON ic.oid = i.indexrelid \
             JOIN pg_namespace n ON n.oid = ic.relnamespace \
             WHERE n.nspname = $1 AND ic.relname = $2",
            &[&schema, &index],
        )
        .await
        .expect("query pg_get_indexdef");
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
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_events", "payload")
            .await
            .as_deref(),
        None,
        "platform-exact json column with no explicit default must have NULL column_default"
    );
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_events", "items")
            .await
            .as_deref(),
        None,
        "second platform-exact json column with no explicit default must have NULL column_default"
    );
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_events", "settings")
            .await
            .as_deref(),
        Some("'{}'::jsonb"),
        "platform-exact json column with an explicit default still renders a DEFAULT \
         clause — proving the confined gate suppresses ONLY the synthesized \
         default-default (emitted when f.default is None), never an author-declared \
         one. NOTE: the author scalar value collapses to '{{}}'::jsonb — an explicit \
         json/object/array default currently normalizes to '{{}}'/'[]'::jsonb across \
         the whole stack (mirrors zeroship-schema json_object_default installSchema \
         parity); end-to-end explicit json default VALUES are a separate systemic gap"
    );
    conn.batch_execute(
        "INSERT INTO zeroship.platform_events (kind, payload, items) \
         VALUES ('boot', '{}'::jsonb, '[]'::jsonb); \
         SELECT id, occurred_at FROM zeroship.platform_events WHERE kind = 'boot';",
    )
    .await
    .expect("insert using both synth defaults");

    reset(&conn, &meta).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_ts_check_expression_surface_round_trips_on_live_pg() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_expr_surface_meta_{tok}");
    reset(&conn, &meta).await;

    let dir = transient_ir_corpus(
        &tok,
        "platform_expr_surface",
        "20260703000000_platform_expr_surface.ts",
        PLATFORM_EXPR_SURFACE_TS,
    );
    let cfg = platform_cfg(dir.path(), &meta, true);
    run_migrate(&cfg)
        .await
        .expect("Platform TS migration with check expression surface applies");

    let expected = [
        (
            "expr_pkce_method_check",
            "CHECK ((pkce_method = 'S256'::text))",
        ),
        (
            "expr_amount_nonnegative",
            "CHECK ((amount_cents >= 0))",
        ),
        (
            "expr_user_id_fmt",
            "CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))",
        ),
        (
            "expr_kind_ok",
            "CHECK ((kind = ANY (ARRAY['a'::text, 'b'::text, 'c'::text])))",
        ),
        (
            "expr_kind_not_reserved",
            "CHECK ((kind <> ALL (ARRAY['x'::text, 'y'::text])))",
        ),
        (
            "expr_data_size",
            "CHECK ((pg_column_size(data) < 262144))",
        ),
        (
            "expr_total_matches",
            "CHECK ((total_cents = (subtotal_cents - credit_cents)))",
        ),
        (
            "expr_floor_nonneg_or_null",
            "CHECK (((floor_cents IS NULL) OR (floor_cents >= 0)))",
        ),
        ("expr_active_visible", "CHECK ((active AND visible))"),
        (
            "expr_expires_window",
            "CHECK ((expires_at <= (created_at + '00:01:00'::interval)))",
        ),
        (
            "expr_snapshot_consistency",
            "CHECK (((status <> ALL (ARRAY['snapshotted'::text, 'snapshotted_suspect'::text])) OR ((snapshot_artifact_path IS NOT NULL) AND (snapshot_sha256 IS NOT NULL) AND (snapshot_ch_version IS NOT NULL))))",
        ),
    ];

    for (constraint, definition) in expected {
        assert_eq!(
            constraint_definition(&conn, "zeroship", "expr_surface", constraint)
                .await
                .as_deref(),
            Some(definition),
            "{constraint} should round-trip through pg_get_constraintdef"
        );
    }

    assert_eq!(
        index_definition(&conn, "zeroship", "expr_status_partial_idx")
            .await
            .as_deref(),
        Some(
            "CREATE INDEX expr_status_partial_idx ON zeroship.expr_surface USING btree (status) WHERE (status <> ALL (ARRAY['snapshotted'::text, 'snapshotted_suspect'::text]))"
        ),
        "notIn partial-index predicate round-trips through pg_get_indexdef"
    );
    assert_eq!(
        index_definition(&conn, "zeroship", "expr_created_desc_idx")
            .await
            .as_deref(),
        Some(
            "CREATE INDEX expr_created_desc_idx ON zeroship.expr_surface USING btree (created_at DESC)"
        ),
        "single-column DESC index round-trips through pg_get_indexdef"
    );
    assert_eq!(
        index_definition(&conn, "zeroship", "expr_user_created_desc_idx")
            .await
            .as_deref(),
        Some(
            "CREATE INDEX expr_user_created_desc_idx ON zeroship.expr_surface USING btree (user_id, created_at DESC)"
        ),
        "mixed ASC/DESC composite index round-trips through pg_get_indexdef"
    );

    reset(&conn, &meta).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_ts_scalar_type_lexicon_round_trips_on_live_pg() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_scalar_types_meta_{tok}");
    reset(&conn, &meta).await;

    let dir = transient_ir_corpus(
        &tok,
        "platform_scalar_types",
        "20260703000000_platform_scalar_types.ts",
        PLATFORM_SCALAR_TYPES_TS,
    );
    let cfg = platform_cfg(dir.path(), &meta, true);
    run_migrate(&cfg)
        .await
        .expect("Platform TS migration with scalar column lexicon applies");

    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_scalar_types", "shard")
            .await
            .as_deref(),
        Some("int2"),
        "t.smallInt() renders as Postgres smallint/int2"
    );
    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_scalar_types", "ratio")
            .await
            .as_deref(),
        Some("float4"),
        "t.real() renders as Postgres real/float4"
    );
    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_scalar_types", "source_ip")
            .await
            .as_deref(),
        Some("inet"),
        "t.inet() renders as Postgres inet"
    );
    assert_eq!(
        column_information_schema_type(&conn, "zeroship", "platform_scalar_types", "scopes")
            .await
            .as_ref()
            .map(|(data_type, udt_name)| (data_type.as_str(), udt_name.as_str())),
        Some(("ARRAY", "_text")),
        "t.textArray() renders as Postgres text[] (information_schema ARRAY/_text)"
    );
    assert_eq!(
        column_udt_name(&conn, "zeroship", "platform_scalar_types", "currency")
            .await
            .as_deref(),
        Some("bpchar"),
        "t.char({{ length: 3 }}) renders as Postgres bpchar"
    );
    assert_eq!(
        column_character_maximum_length(&conn, "zeroship", "platform_scalar_types", "currency")
            .await,
        Some(3),
        "t.char({{ length: 3 }}) preserves character_maximum_length=3"
    );
    assert_eq!(
        column_default_expr(&conn, "zeroship", "platform_scalar_types", "currency")
            .await
            .as_deref(),
        Some("'usd'::bpchar"),
        "t.char({{ length: 3 }}).default(\"usd\") round-trips through pg_get_expr as bpchar"
    );

    reset(&conn, &meta).await;
    global_lock.release().await;
}

#[compio::test]
async fn platform_ts_domain_column_round_trips_on_live_pg() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_domain_column_meta_{tok}");
    reset(&conn, &meta).await;

    let dir = transient_ir_corpus(
        &tok,
        "platform_domain_column",
        "20260703000000_platform_domain_column.ts",
        PLATFORM_DOMAIN_COLUMN_TS,
    );
    let cfg = platform_cfg(dir.path(), &meta, true);
    run_migrate(&cfg)
        .await
        .expect("Platform TS migration with t.domain() column applies");

    let domain_def = domain_constraint_definition(&conn, "zeroship", "myd")
        .await
        .expect("domain check constraint exists");
    assert!(
        domain_def.contains("VALUE = ANY (ARRAY['a'::text, 'b'::text])"),
        "domain myd must carry its membership CHECK, got: {domain_def}"
    );

    assert_eq!(
        column_domain_information_schema(&conn, "zeroship", "domain_surface", "state")
            .await
            .as_ref()
            .map(|(domain_name, domain_schema, udt_name)| (
                domain_name.as_deref(),
                domain_schema.as_deref(),
                udt_name.as_str()
            )),
        Some((Some("myd"), Some("zeroship"), "text")),
        "information_schema must expose t.domain(\"myd\") through domain_name/domain_schema; \
         Postgres reports the underlying base type in udt_name for domain columns"
    );
    assert_eq!(
        column_catalog_type_name(&conn, "zeroship", "domain_surface", "state")
            .await
            .as_ref()
            .map(|(type_schema, type_name)| (type_schema.as_str(), type_name.as_str())),
        Some(("zeroship", "myd")),
        "pg_catalog must show the column's physical type as zeroship.myd"
    );

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
        table_columns(&conn, "zeroship", "platform_apps").await,
        vec!["id".to_string(), "created_at".to_string()],
        "platform CreateTable preserves the author column order on the FK target"
    );
    assert_eq!(
        table_columns(&conn, "zeroship", "platform_registry").await,
        vec![
            "app_id".to_string(),
            "route".to_string(),
            "target".to_string(),
            "status".to_string(),
            "created_at".to_string(),
        ],
        "platform CreateTable preserves the author column order, with no confined system fields"
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
        "setRls attached to the platform-exact table"
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
async fn platform_ts_composite_and_non_id_fks_round_trip_on_live_pg() {
    ensure_dedicated_db().await;
    let global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let tok = token();
    let meta = format!("platform_fk_meta_{tok}");
    reset(&conn, &meta).await;

    let dir = transient_ir_corpus(
        &tok,
        "platform_composite_fk",
        "20260703000000_platform_composite_fk.ts",
        PLATFORM_COMPOSITE_FK_TS,
    );
    let cfg = platform_cfg(dir.path(), &meta, true);
    run_migrate(&cfg)
        .await
        .expect("Platform TS migration with non-id and composite FKs applies");

    assert_eq!(
        constraint_definition(
            &conn,
            "zeroship",
            "app_oauth_clients",
            "app_oauth_clients_client_id_fkey",
        )
        .await
        .as_deref(),
        Some(
            "FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id) ON DELETE CASCADE"
        ),
        "non-id FK target column round-trips through pg_get_constraintdef \
         (pg_get_constraintdef unqualifies the target schema when it is in search_path; \
         the constraint is bound to zeroship.oauth_clients by OID)"
    );
    assert_eq!(
        constraint_definition(
            &conn,
            "zeroship",
            "billing_line_provider_refs",
            "billing_line_provider_refs_line_fk",
        )
        .await
        .as_deref(),
        Some(
            "FOREIGN KEY (invoice_id, app_id, segment_no) REFERENCES invoice_lines(invoice_id, app_id, segment_no) ON DELETE CASCADE"
        ),
        "composite FK column lists round-trip through pg_get_constraintdef \
         (target schema unqualified by search_path; bound to zeroship.invoice_lines by OID)"
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
