// Byte-identity oracle: the fluent `@zeroship/migrate` authoring surface records
// the SAME ops the engine's embedded recorder (`migrate_ops.js`) committed into
// the golden corpus. The npm `ops.ts` and the V8-embedded `migrate_ops.js` are
// two implementations of the same locked fluent surface; this test re-authors a
// golden fixture's `up()` through `table()` and asserts the recorded op list
// equals the committed golden `.golden.json`'s `ops` — proving the fluent-only
// redesign is PURE SUGAR (the recorded IR is byte-identical to the pre-redesign
// golden, except the C1 FK-actions delta which the FK goldens carry).
//
// Re-bless note: `fluent_ddl`'s `label` column was authored via the now-removed
// `t.string()` alias (wire `string`). The spec removes that alias (canonical
// `text`/`integer`), so `label` is re-authored as `t.text()` and the golden's
// `label` type re-blessed `string` → `text`. This is the ONLY byte change beyond
// C1 — a direct consequence of the mandated alias removal (`t.string`/`t.int`).

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { t, table } from "../src/index.js";
import { pg } from "../src/pg.js";
import { __begin, __drain } from "../src/ops.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixturesDir = resolve(here, "../../../crates/zeroship-migrate/tests/op_fixtures");

async function golden(stem: string): Promise<any> {
  return JSON.parse(await readFile(resolve(fixturesDir, `${stem}.golden.json`), "utf8"));
}

/** Normalize a `createTable` op for parity comparison: the committed golden is the
 *  RUST RE-SERIALIZATION of the typed IR, which fills `constraints`/`indexes` with
 *  serde defaults (`[]`) on serialize; the fluent recorder OMITS empty arrays. Both
 *  deserialize to the same typed op (absent == empty), so we drop empty
 *  `constraints`/`indexes` on both sides before comparing. */
function normalizeOps(ops: any[]): any[] {
  return ops.map((op) => {
    if (op.op !== "createTable") return op;
    const out = { ...op };
    if (Array.isArray(out.constraints) && out.constraints.length === 0) delete out.constraints;
    if (Array.isArray(out.indexes) && out.indexes.length === 0) delete out.indexes;
    return out;
  });
}

function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

test("fluent_ddl fluent-recorded ops equal the committed golden", async () => {
  const ops = record(() => {
    table("accounts").create({
      columns: {
        id: t.id(),
        email: t.text().notNull().unique(),
        balance: t.numeric(12, 2).notNull().default({ decimal: "0.00" }),
        created_at: t.timestamp().notNull().default({ fn: "now" }),
        external_id: t.uuid(),
        avatar: t.bytes(),
        active: t.boolean().notNull().default(true),
        profile: t.json(),
        owner: t.ref("users"),
        embedding: t.vector(1536),
        location: t.geoPoint(),
        // re-blessed string → text (t.string alias removed, §7).
        label: t.text(),
        hits: t.integer().notNull().default(0),
        big_hits: t.bigInt(),
        ratio: t.float(),
        secret: t.encrypted({ of: t.text() }),
      },
    });
    table("memberships").create({
      columns: { account_id: t.uuid().notNull(), team: t.text().notNull() },
      primaryKey: ["account_id", "team"],
      uniques: [{ name: "memberships_team_uq", columns: ["team"] }],
      checks: [{ name: "memberships_team_chk", expr: (c) => c("team").isNotNull() }],
      foreignKeys: [
        {
          name: "memberships_account_fk",
          columns: ["account_id"],
          references: { table: "accounts", columns: ["id"] },
        },
      ],
      indexes: [{ name: "memberships_account_idx", columns: ["account_id"] }],
    });
    table("accounts").column("status").add({ type: t.text().notNull().default("new") });
    table("memberships").foreignKey("memberships_team_fk").add({
      columns: ["team"],
      references: { table: "teams", columns: ["name"] },
    });
    table("accounts").unique("accounts_external_uq").add({ columns: ["external_id"] });
    table("accounts").check("accounts_balance_chk").add({ expr: (c) => c("balance").ge(0) });
    table("accounts").constraint("accounts_legacy_chk").drop();
    table("accounts").column("balance").alter({ type: t.numeric(14, 2) });
    table("accounts").column("profile").alter({ nullable: false });
    table("accounts").column("label").rename({ to: "display_label", type: t.text() });
    table("accounts").index("accounts_active_email_idx").add({
      columns: ["email"],
      unique: true,
      where: (c) => c("active").isTrue(),
    });
    table("accounts").column("nickname").add({ type: t.text() });
    table("accounts").column("nickname").alter({ nullable: false });
  });
  const g = await golden("fluent_ddl");
  assert.deepEqual(normalizeOps(ops), normalizeOps(g.ops));
});

test("fluent_dml fluent-recorded ops equal the committed golden", async () => {
  const ops = record(() => {
    table("status_codes").insert({
      rows: [
        { code: 200, label: "ok" },
        { code: 404, label: "not found" },
      ],
    });
    table("status_codes").update({
      set: {
        label: (c) => c.fn.coalesce(c("label"), "unknown"),
        norm: (c) => c.fn.lower(c.fn.trim(c("label"))),
        shout: (c) => c.fn.upper(c("label")),
        len: (c) => c.fn.length(c("label")),
        mag: (c) => c.fn.abs(c("code").sub(500)),
        canon: (c) => c.fn.nullif(c("label"), ""),
        score: (c) => c("code").add(1).mul(2).sub(3).div(1),
        joined: (c) => c("label").concat(" ", c("code").cast("text")),
        code_txt: (c) => c("code").cast("text"),
      },
      where: (c) => c("code").gt(0).and(c("label").isNotNull()),
    });
    table("status_codes").del({
      where: (c) =>
        c("code")
          .ne(0)
          .or(c("code").le(0))
          .or(c("code").ge(999))
          .or(c("label").isNull())
          .or(c("active").isFalse())
          .and(
            c.fn
              .case([[c("code").lt(100), c("code").isNull()]], c("label").isNull())
              .isTrue(),
          ),
      limit: 100,
    });
    table("status_codes").backfill({
      set: {
        full: (c) => c.fn.concatWs(" ", c("label"), c("code").cast("text")),
        first: (c) => c.fn.splitPart(c("label"), " ", 1),
        touched: (c) => c.fn.now(),
        token: (c) => c.fn.genRandomUuid(),
      },
      where: (c) => c("code").gt(0),
      cursorColumn: "code",
      batchSize: 500,
      name: "fluent_backfill",
    });
  });
  const g = await golden("fluent_dml");
  assert.deepEqual(normalizeOps(ops), normalizeOps(g.ops));
});

// Recorder lock-step for the table-rename follow-up: the TS `table().rename({ to })`
// surface records the SAME `renameTable` ops the V8 `migrate_ops.js` recorder
// committed into the `ddl_rename_table` golden — a bare rename AND a schema+ifExists
// rename. The byte-identity oracle for the new `Op::RenameTable` variant across both
// fluent impls (RED before `rename()` existed on the handle).
test("ddl_rename_table fluent-recorded ops equal the committed golden", async () => {
  const ops = record(() => {
    table("accounts").rename({ to: "members" });
    table("orders").rename({ to: "purchases", ifExists: true, schema: "reporting" });
  });
  const g = await golden("ddl_rename_table");
  assert.deepEqual(normalizeOps(ops), normalizeOps(g.ops));
});

test("pg_vendor typed pg surface records ops equal the committed golden", async () => {
  const ops = record(() => {
    pg.createExtension({ name: "citext", ifNotExists: true });
    pg.dropExtension({ name: "citext", ifExists: true });
    pg.createSchema({ name: "zeroship", ifNotExists: true });
    pg.dropSchema({ name: "zeroship", ifExists: true, cascade: true });

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

    pg.createFunction({
      name: "audit_events_block_tamper",
      schema: "zeroship",
      returns: "trigger",
      language: "plpgsql",
      replace: true,
      body: "BEGIN RAISE EXCEPTION 'audit_events is append-only'; END;",
    });

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

    pg.raw({
      sql: "SELECT set_config('zeroship.tenant_app', 'app_demo', false)",
      reason: "set tenant app GUC for pg vendor fixture",
    });
  });
  const g = await golden("pg_vendor");
  assert.deepEqual(normalizeOps(ops), normalizeOps(g.ops));
});
