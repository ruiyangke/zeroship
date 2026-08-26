// Byte-identity oracle: the fluent `@zeroship/migrate` authoring surface records
// the SAME author ops the engine's embedded recorder (`migrate_ops.js`) committed
// before the Rust build path resolves profile-owned table shape into the golden
// corpus. The npm `ops.ts` and the V8-embedded `migrate_ops.js` are two
// implementations of the same locked fluent surface; this test re-authors a
// golden fixture's `up()` through `table()` and asserts the post-policy op list
// equals the committed golden `.golden.json`'s `ops`.
//
// WHAT THIS TEST MATCHES. The committed `<stem>.golden.json` is the CONTRACT
// (`src/types.ts:11-12`). The `<stem>.mig.js` sitting beside it is a HINT about
// the intended authoring, not a harness: nothing executes it (the engine's own
// support-matrix runner skips every non-`.golden.json` file and its JS parity
// test inlines its authoring), so the two are tied by filename alone. When the
// authoring below is updated, it is derived from the GOLDEN and only sanity-read
// against the `.mig.js`; a disagreement between them is an upstream finding, not
// a licence to match the input file.
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

import {
  decimal,
  t,
  table,
  now,
  uuidV4,
  currentSetting,
  concatWs,
  createFunction,
  dropFunction,
  dropOwnedBy,
  extension,
  grant,
  raw,
  revoke,
  role,
  schema,
} from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixturesDir = resolve(here, "../../../crates/zero-migrate/tests/op_fixtures");

async function golden(stem: string): Promise<any> {
  return JSON.parse(await readFile(resolve(fixturesDir, `${stem}.golden.json`), "utf8"));
}

/** Normalize a `createTable` op for parity comparison: the committed golden is the
 *  RUST RE-SERIALIZATION of the typed IR, which fills `constraints`/`indexes` with
 *  serde defaults (`[]`) and serializes unresolved `primaryKey` as null; the fluent
 *  recorder omits defaulted/unresolved fields. Both deserialize to the same typed
 *  op, so we drop defaulted fields on both sides before comparing. */
function normalizeOps(ops: any[]): any[] {
  return ops.map((op) => {
    if (op.op !== "createTable") return op;
    const out = { ...op };
    if (Array.isArray(out.constraints) && out.constraints.length === 0) delete out.constraints;
    if (Array.isArray(out.indexes) && out.indexes.length === 0) delete out.indexes;
    if (out.primaryKey === null) delete out.primaryKey;
    return out;
  });
}

// This list MIRRORS the engine's confined injection; it does not define it. The
// charter declares these columns with SQL-ish tokens (`text`, `timestamptz`,
// `integer`) and the engine's `inject_column_to_ir`
// (.../model/table_shape.rs, the `match column.ty`) is
// what those tokens MEAN. Read that function, not the charter, when this drifts.
//
// The three string columns are BOUNDED `string(255)`, not `text`, and that is
// deliberate upstream: they hold ids and are keyed (`id` is the primary key,
// `created_by` carries an audit index), and MySQL cannot key an unbounded TEXT.
// This mirror said `text` for all three and was WRONG - the golden was right.
// Established 2026-08-10 by escalating the disagreement to zero-migrate rather
// than re-blessing the shared fixture; they confirmed the bound is intended and
// pinned it in rendered DDL on an independent path.
const confinedSystemColumns = [
  { name: "id", type: { string: { length: 255 } }, nullable: false },
  { name: "created_at", type: "timestamp", nullable: false },
  { name: "updated_at", type: "timestamp", nullable: false },
  { name: "created_by", type: { string: { length: 255 } }, nullable: true },
  { name: "updated_by", type: { string: { length: 255 } }, nullable: true },
  { name: "version", type: "int", nullable: false },
  { name: "deleted_at", type: "timestamp", nullable: true },
];

const confinedSystemColumnNames = new Set(confinedSystemColumns.map((column) => column.name));
const confinedSystemIndexes = [
  { columns: [{ kind: "column", name: "deleted_at" }] },
  { columns: [{ kind: "column", name: "updated_at" }] },
  { columns: [{ kind: "column", name: "created_by" }] },
];

function resolveConfinedCreateTables(ops: any[]): any[] {
  return ops.map((op) => {
    if (op.op !== "createTable") return op;

    let foldedId = false;
    const authorColumns = [];
    for (const col of op.columns) {
      if (col.name === "id" && col.type === "uuid") {
        foldedId = true;
        continue;
      }
      if (confinedSystemColumnNames.has(col.name)) {
        throw new Error(`unexpected confined system-column collision in JS parity fixture: ${col.name}`);
      }
      authorColumns.push(col);
    }
    if (op.primaryKey !== undefined && op.primaryKey !== null) {
      const authorPkIsFoldedId = foldedId && op.primaryKey.length === 1 && op.primaryKey[0] === "id";
      if (!authorPkIsFoldedId) {
        throw new Error("unexpected author primaryKey in confined JS parity fixture");
      }
    }

    return {
      ...op,
      columns: [...confinedSystemColumns, ...authorColumns],
      primaryKey: ["id"],
      indexes: [...(op.indexes ?? []), ...confinedSystemIndexes],
    };
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
        balance: t.numeric({ precision: 12, scale: 2 }).notNull().default(decimal("0.00")),
        authored_at: t.timestamp().notNull().default(now()),
        external_id: t.uuid(),
        avatar: t.bytes(),
        active: t.boolean().notNull().default(true),
        profile: t.json(),
        // The golden records `{"name":"owner","type":"text","references":{"table":
        // "users","column":"id"}}` — a fully specified local type PLUS the typed
        // single-column FK facet. `t.ref("users")` would record the table-only
        // `{ ref: { references: "users" } }` column TYPE instead: a different
        // construct that drops the target column.
        owner: t.text().references("users", "id"),
        embedding: t.vector({ dimensions: 1536 }),
        location: t.geoPoint(),
        // re-blessed string → text (t.string alias removed, §7).
        label: t.text(),
        hits: t.int().notNull().default(0),
        big_hits: t.bigInt(),
        ratio: t.double(),
        secret: t.encrypted({ of: t.text() }),
      },
    });
    table("memberships").create({
      columns: { account_id: t.uuid().notNull(), team: t.text().notNull() },
      uniques: [{ name: "memberships_team_uq", columns: ["team"] }],
      checks: [{ name: "memberships_team_chk", expr: (col) => col("team").isNotNull() }],
      foreignKeys: [
        {
          name: "memberships_account_fk",
          columns: ["account_id"],
          references: { table: "accounts", columns: ["id"] },
        },
      ],
      indexes: [{ name: "memberships_account_idx", on: ["account_id"] }],
    });
    table("accounts").column("status").add({ type: t.text().notNull().default("new") });
    table("memberships").foreignKey("memberships_team_fk").add({
      columns: ["team"],
      references: { table: "teams", columns: ["name"] },
    });
    table("accounts").unique("accounts_external_uq").add({ columns: ["external_id"] });
    table("accounts").check("accounts_balance_chk").add({ expr: (col) => col("balance").ge(0) });
    table("accounts").constraint("accounts_legacy_chk").drop();
    table("accounts").column("balance").setType({ to: t.numeric({ precision: 14, scale: 2 }) });
    table("accounts").column("profile").setNotNull();
    table("accounts").column("label").rename({ to: "display_label", type: t.text() });
    table("accounts").index("accounts_active_email_idx").add({
      on: ["email"],
      unique: true,
      where: (col) => col("active").isTrue(),
    });
    table("accounts").column("nickname").add({ type: t.text() });
    table("accounts").column("nickname").setNotNull();
  });
  const g = await golden("fluent_ddl");
  assert.deepEqual(normalizeOps(resolveConfinedCreateTables(ops)), normalizeOps(g.ops));
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
        label: (col) => col("label").coalesce("unknown"),
        norm: (col) => col("label").trim().lower(),
        shout: (col) => col("label").upper(),
        len: (col) => col("label").length(),
        mag: (col) => col("code").sub(500).abs(),
        canon: (col) => col("label").nullif(""),
        score: (col) => col("code").add(1).mul(2).sub(3).div(1),
        joined: (col) => col("label").concat(" ", col("code").cast({ to: "text" })),
        code_txt: (col) => col("code").cast({ to: "text" }),
      },
      where: (col) => col("code").gt(0).and(col("label").isNotNull()),
    });
    table("status_codes").delete({
      where: (col) =>
        col("code")
          .ne(0)
          .or(col("code").le(0))
          .or(col("code").ge(999))
          .or(col("label").isNull())
          .or(col("active").isFalse())
          .and(
            col
              .case({ branches: [{ when: col("code").lt(100), then: col("code").isNull() }], else: col("label").isNull() })
              .isTrue(),
          ),
      limit: 100,
    });
    table("status_codes").backfill({
      set: {
        full: (col) => concatWs(" ", col("label"), col("code").cast({ to: "text" })),
        first: (col) => col("label").splitPart(" ", 1),
        touched: now(),
        token: uuidV4(),
      },
      where: (col) => col("code").gt(0),
      // The golden records the ORDERED cursor tuple plus an explicit stability
      // mode; neither has a default in the engine's `backfill` op.
      cursorColumns: ["code"],
      cursorStability: { mode: "guardUpdates" },
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
    extension("citext").create({ ifNotExists: true });
    extension("citext").drop({ ifExists: true });
    schema("zero_migrate").create({ ifNotExists: true });
    schema("zero_migrate").drop({ ifExists: true, cascade: true });

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

    table("events", { schema: "zero_migrate" }).partition("events_2026_11").attach({
      from: ["2026-11-01T00:00:00Z"],
      to: ["2026-12-01T00:00:00Z"],
    });

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

    createFunction({
      name: "audit_events_block_tamper",
      schema: "zero_migrate",
      returns: "trigger",
      language: "plpgsql",
      replace: true,
      body: "BEGIN RAISE EXCEPTION 'audit_events is append-only'; END;",
    });

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

    raw({
      sql: "SELECT set_config('zero_migrate.tenant_app', 'app_demo', false)",
      reason: "set tenant app GUC for pg vendor fixture",
    });
  });
  const g = await golden("pg_vendor");
  assert.deepEqual(normalizeOps(ops), normalizeOps(g.ops));
});
