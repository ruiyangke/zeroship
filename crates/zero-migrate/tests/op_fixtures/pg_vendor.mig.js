// op.* VENDOR fixture (`zero-migrate`) — the variant-exhaustiveness +
// cross-impl round-trip gate for the privileged Postgres primitives.
// Exercises EVERY vendor Op variant at least once, modelled on the
// platform's own 0025_roles_rls / 0001_extensions_schemas / 0002_auth constructs,
// so the JS recorder's vendor named exports + table-handle augmentations stay
// byte-identical to the Rust `Op` wire shape.
import {
  table,
  currentSetting,
  createFunction,
  dropFunction,
  dropOwnedBy,
  extension,
  grant,
  raw,
  revoke,
  role,
  schema,
} from "zero-migrate";

export const name = "pg_vendor";

export function up() {
  // ── extensions + schemas (0001) ──
  extension("citext").create({ ifNotExists: true });
  extension("citext").drop({ ifExists: true });
  schema("zero_migrate").create({ ifNotExists: true });
  schema("zero_migrate").drop({ ifExists: true, cascade: true });

  // ── roles (0025) ──
  role("zero_migrate_auth").create({
    login: true,
    password: "zero_migrate_auth",
    bypassRls: true,
    setSearchPath: ["zero_migrate", "public"],
    ifNotExists: true,
  });
  role("zero_migrate_auth").setOptions({ setSearchPath: ["zero_migrate", "public"] });
  role("zero_migrate_auth").drop({ ifExists: true });
  dropOwnedBy({ roles: ["zero_migrate_auth"] });

  // ── grants / revokes (0025 / 0004) ──
  grant({
    privileges: ["select", "insert", "update", "delete"],
    on: { kind: "table", names: ["users"], schema: "zero_migrate" },
    to: ["zero_migrate_auth"],
  });
  revoke({
    privileges: ["update", "delete", "truncate"],
    on: { kind: "table", names: ["audit_events"], schema: "zero_migrate" },
    from: ["public"],
  });

  // ── partition attach (PG vendor; distinct from createPartition) ──
  table("events", { schema: "zero_migrate" }).partition("events_2026_11").attach({
    from: ["2026-11-01T00:00:00Z"],
    to: ["2026-12-01T00:00:00Z"],
  });

  // ── RLS + policies (0025) ──
  const secrets = table("app_secrets", { schema: "zero_migrate" });
  secrets.setRls({ enabled: true, forced: true });
  secrets.policy("tenant_isolation").create({
    for: "all",
    using: (col) =>
      col("app_id").eq(currentSetting("zero_migrate.tenant_app", { missingOk: true }).cast({ to: "text" })),
    withCheck: (col) =>
      col("app_id").eq(currentSetting("zero_migrate.tenant_app", { missingOk: true }).cast({ to: "text" })),
  });
  secrets.policy("tenant_isolation").drop({ ifExists: true });
  secrets.setRls({ enabled: false, forced: false });

  // ── functions (0002 tamper trigger) ──
  createFunction({
    name: "audit_events_block_tamper",
    schema: "zero_migrate",
    returns: "trigger",
    language: "plpgsql",
    replace: true,
    body: "BEGIN RAISE EXCEPTION 'audit_events is append-only'; END;",
  });

  // ── triggers (0002 + A2) ──
  const audit = table("audit_events", { schema: "zero_migrate" });
  audit.trigger("audit_events_block_update").create({
    timing: "before",
    events: ["update", "delete"],
    forEach: "row",
    execute: "audit_events_block_tamper",
    when: (col) => col("app_id").isNotNull(),
  });
  audit.trigger("audit_events_append_only").create({
    timing: "before",
    events: ["update"],
    forEach: "row",
    body: (b) => [b.raise({ level: "abort", message: "append-only", errcode: "P0001" })],
  });
  audit.trigger("audit_events_block_update").drop({ ifExists: true });

  dropFunction({
    name: "audit_events_block_tamper",
    schema: "zero_migrate",
    ifExists: true,
  });

  // ── the gated raw escape ──
  raw({
    sql: "SELECT set_config('zero_migrate.tenant_app', 'app_demo', false)",
    reason: "set tenant app GUC for pg vendor fixture",
  });
}
