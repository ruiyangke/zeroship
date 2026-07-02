// op.* VENDOR fixture (`@zeroship/migrate/pg`) — the variant-exhaustiveness +
// cross-impl round-trip gate for the privileged Postgres primitives (vendor spec
// §4.5). Exercises EVERY vendor Op variant at least once, modelled on the
// platform's own 0025_roles_rls / 0001_extensions_schemas / 0002_auth constructs,
// so the JS recorder's `pg.*` namespace + table-handle augmentations stay
// byte-identical to the Rust `Op` wire shape.
import { table } from "@zeroship/migrate";
import { pg } from "@zeroship/migrate/pg";

export const name = "pg_vendor";

export function up() {
  // ── extensions + schemas (0001) ──
  pg.createExtension({ name: "citext", ifNotExists: true });
  pg.dropExtension({ name: "citext", ifExists: true });
  pg.createSchema({ name: "zeroship", ifNotExists: true });
  pg.dropSchema({ name: "zeroship", ifExists: true, cascade: true });

  // ── roles (0025) ──
  pg.createRole({
    name: "zeroship_auth",
    login: true,
    password: "zeroship_auth",
    bypassRls: true,
    setSearchPath: ["zeroship", "public"],
    ifNotExists: true,
  });
  pg.alterRole({ name: "zeroship_auth", setSearchPath: ["zeroship", "public"] });
  pg.dropRole({ name: "zeroship_auth", ifExists: true });
  pg.dropOwnedBy({ roles: ["zeroship_auth"] });

  // ── grants / revokes (0025 / 0004) ──
  pg.grant({
    privileges: ["select", "insert", "update", "delete"],
    on: { kind: "table", names: ["users"], schema: "zeroship" },
    to: ["zeroship_auth"],
  });
  pg.revoke({
    privileges: ["update", "delete", "truncate"],
    on: { kind: "table", names: ["audit_events"], schema: "zeroship" },
    from: ["public"],
  });

  // ── RLS + policies (0025) ──
  const secrets = table("app_secrets", { schema: "zeroship" });
  secrets.enableRowLevelSecurity();
  secrets.forceRowLevelSecurity();
  secrets.createPolicy({
    name: "tenant_isolation",
    for: "all",
    using: (c) =>
      c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("text")),
    withCheck: (c) =>
      c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("text")),
  });
  secrets.dropPolicy({ name: "tenant_isolation", ifExists: true });
  secrets.disableRowLevelSecurity();
  secrets.noForceRowLevelSecurity();

  // ── functions (0002 tamper trigger) ──
  pg.createFunction({
    name: "audit_events_block_tamper",
    schema: "zeroship",
    returns: "trigger",
    language: "plpgsql",
    replace: true,
    body: "BEGIN RAISE EXCEPTION 'audit_events is append-only'; END;",
  });

  // ── triggers (0002 + A2) ──
  const audit = table("audit_events", { schema: "zeroship" });
  audit.createTrigger({
    name: "audit_events_block_update",
    timing: "before",
    events: ["update", "delete"],
    forEach: "row",
    execute: "audit_events_block_tamper",
    when: (c) => c("app_id").isNotNull(),
  });
  audit.createTrigger({
    name: "audit_events_append_only",
    timing: "before",
    events: ["update"],
    forEach: "row",
    body: (b) => [b.raise({ level: "abort", message: "append-only", errcode: "P0001" })],
  });
  audit.dropTrigger({ name: "audit_events_block_update", ifExists: true });

  pg.dropFunction({
    name: "audit_events_block_tamper",
    schema: "zeroship",
    ifExists: true,
  });

  // ── the gated raw escape (vendor spec §2.11) ──
  pg.raw({
    sql: "SELECT set_config('zeroship.tenant_app', 'app_demo', false)",
    reason: "set tenant app GUC for pg vendor fixture",
  });
}
