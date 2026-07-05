// op.* VENDOR fixture (`@zeroship/migrate/pg`) — the variant-exhaustiveness +
// cross-impl round-trip gate for the privileged Postgres primitives (vendor spec
// §4.5). Exercises EVERY vendor Op variant at least once, modelled on the
// platform's own 0025_roles_rls / 0001_extensions_schemas / 0002_auth constructs,
// so the JS recorder's vendor named exports + table-handle augmentations stay
// byte-identical to the Rust `Op` wire shape.
import { table } from "@zeroship/migrate";
import {
  alterRole,
  createFunction,
  dropExtension,
  dropFunction,
  dropOwnedBy,
  dropRole,
  dropSchema,
  extension,
  grant,
  pgTable,
  raw,
  revoke,
  role,
  schema,
} from "@zeroship/migrate/pg";

export const name = "pg_vendor";

export function up() {
  // ── extensions + schemas (0001) ──
  extension({ name: "citext", ifNotExists: true });
  dropExtension({ name: "citext", ifExists: true });
  schema({ name: "zeroship", ifNotExists: true });
  dropSchema({ name: "zeroship", ifExists: true, cascade: true });

  // ── roles (0025) ──
  role({
    name: "zeroship_auth",
    login: true,
    password: "zeroship_auth",
    bypassRls: true,
    setSearchPath: ["zeroship", "public"],
    ifNotExists: true,
  });
  alterRole({ name: "zeroship_auth", setSearchPath: ["zeroship", "public"] });
  dropRole({ name: "zeroship_auth", ifExists: true });
  dropOwnedBy({ roles: ["zeroship_auth"] });

  // ── grants / revokes (0025 / 0004) ──
  grant({
    privileges: ["select", "insert", "update", "delete"],
    on: { kind: "table", names: ["users"], schema: "zeroship" },
    to: ["zeroship_auth"],
  });
  revoke({
    privileges: ["update", "delete", "truncate"],
    on: { kind: "table", names: ["audit_events"], schema: "zeroship" },
    from: ["public"],
  });

  // ── partition attach (PG vendor; distinct from createPartition) ──
  pgTable("events", { schema: "zeroship" }).partition("events_2026_11").attach({
    from: ["2026-11-01T00:00:00Z"],
    to: ["2026-12-01T00:00:00Z"],
  });

  // ── RLS + policies (0025) ──
  const secrets = pgTable("app_secrets", { schema: "zeroship" });
  secrets.setRls({ enabled: true, forced: true });
  secrets.policy("tenant_isolation").create({
    for: "all",
    using: (c) =>
      c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("text")),
    withCheck: (c) =>
      c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("text")),
  });
  secrets.policy("tenant_isolation").drop({ ifExists: true });
  secrets.setRls({ enabled: false, forced: false });

  // ── functions (0002 tamper trigger) ──
  createFunction({
    name: "audit_events_block_tamper",
    schema: "zeroship",
    returns: "trigger",
    language: "plpgsql",
    replace: true,
    body: "BEGIN RAISE EXCEPTION 'audit_events is append-only'; END;",
  });

  // ── triggers (0002 + A2) ──
  const audit = table("audit_events", { schema: "zeroship" });
  audit.trigger("audit_events_block_update").create({
    timing: "before",
    events: ["update", "delete"],
    forEach: "row",
    execute: "audit_events_block_tamper",
    when: (c) => c("app_id").isNotNull(),
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
    schema: "zeroship",
    ifExists: true,
  });

  // ── the gated raw escape (vendor spec §2.11) ──
  raw({
    sql: "SELECT set_config('zeroship.tenant_app', 'app_demo', false)",
    reason: "set tenant app GUC for pg vendor fixture",
  });
}
