// PR11 — the eager fluent `table()` facade. The facade is PURE SUGAR over the flat
// op-functions: `table("users",{schema}).addColumn(…)` must record the IDENTICAL op
// the flat `addColumn("users",…,{schema})` records. These tests are the HEADLINE
// byte-identical-IR invariant:
//
//   (A) facade ≡ flat, per op — author the same op both ways, assert deepEqual.
//   (B) full-migration parity against a committed golden — re-author a golden
//       fixture's up() entirely through table() and assert it lowers to the
//       byte-identical committed .ir.json.
//
// Plus the behavioral pins the task mandates: EAGER recording (no terminal),
// schema propagation + per-method override precedence, guard pass-through.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  addCheck,
  addColumn,
  addForeignKey,
  addUnique,
  alterColumn,
  backfill,
  createIndex,
  createTable,
  del,
  dropColumn,
  dropConstraint,
  dropIndex,
  dropTable,
  insert,
  renameColumn,
  table,
  t,
  update,
} from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixturesDir = resolve(here, "../../../crates/zeroship-migrate-js/tests/op_fixtures");

async function golden(stem: string): Promise<any> {
  return JSON.parse(await readFile(resolve(fixturesDir, `${stem}.ir.json`), "utf8"));
}

/** The committed golden is the RUST re-serialization of the typed IR (fills empty
 *  `constraints`/`indexes` with `[]`); the npm DSL OMITS empty arrays. Drop them on
 *  both sides before comparing — identical to golden-parity.test.ts. */
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

// ───────────────────────────────────────────────────────────────────────────
// (A) facade ≡ flat, per op. For each handle method, record the op authored via
// table(...).method(...) and via the equivalent flat op(...), assert deepEqual.
// ───────────────────────────────────────────────────────────────────────────

test("(A) facade ≡ flat — every handle method records the identical op", () => {
  const cases: { name: string; via: () => any[]; flat: () => any[] }[] = [
    {
      name: "create",
      via: () =>
        record(() =>
          table("orders").create({ id: t.id(), total: t.numeric(12, 2).notNull() }),
        ),
      flat: () =>
        record(() =>
          createTable("orders", { id: t.id(), total: t.numeric(12, 2).notNull() }),
        ),
    },
    {
      name: "create (build + opts)",
      via: () =>
        record(() =>
          table("memberships").create(
            { account_id: t.uuid().notNull(), team: t.text().notNull() },
            (b) => {
              b.primaryKey(["account_id", "team"]);
              b.unique(["team"], { name: "memberships_team_uq" });
              b.index(["account_id"], { name: "memberships_account_idx" });
            },
            { ifNotExists: true },
          ),
        ),
      flat: () =>
        record(() =>
          createTable(
            "memberships",
            { account_id: t.uuid().notNull(), team: t.text().notNull() },
            (b) => {
              b.primaryKey(["account_id", "team"]);
              b.unique(["team"], { name: "memberships_team_uq" });
              b.index(["account_id"], { name: "memberships_account_idx" });
            },
            { ifNotExists: true },
          ),
        ),
    },
    {
      name: "drop",
      via: () => record(() => table("scratch").drop({ cascade: true })),
      flat: () => record(() => dropTable("scratch", { cascade: true })),
    },
    {
      name: "addColumn",
      via: () => record(() => table("users").addColumn("status", t.text().notNull().default("new"))),
      flat: () => record(() => addColumn("users", "status", t.text().notNull().default("new"))),
    },
    {
      name: "dropColumn",
      via: () => record(() => table("users").dropColumn("legacy")),
      flat: () => record(() => dropColumn("users", "legacy")),
    },
    {
      name: "renameColumn",
      via: () => record(() => table("users").renameColumn("label", "display_label", t.text())),
      flat: () => record(() => renameColumn("users", "label", "display_label", t.text())),
    },
    {
      name: "alterColumn (type)",
      via: () => record(() => table("users").alterColumn("balance", { type: t.numeric(14, 2) })),
      flat: () => record(() => alterColumn("users", "balance", { type: t.numeric(14, 2) })),
    },
    {
      name: "alterColumn (nullable)",
      via: () => record(() => table("users").alterColumn("profile", { nullable: false })),
      flat: () => record(() => alterColumn("users", "profile", { nullable: false })),
    },
    {
      name: "addForeignKey",
      via: () =>
        record(() =>
          table("memberships").addForeignKey({
            columns: ["team"],
            references: { table: "teams", columns: ["name"] },
            name: "memberships_team_fk",
          }),
        ),
      flat: () =>
        record(() =>
          addForeignKey("memberships", {
            columns: ["team"],
            references: { table: "teams", columns: ["name"] },
            name: "memberships_team_fk",
          }),
        ),
    },
    {
      name: "addUnique",
      via: () =>
        record(() => table("accounts").addUnique({ columns: ["external_id"], name: "accounts_external_uq" })),
      flat: () =>
        record(() => addUnique("accounts", { columns: ["external_id"], name: "accounts_external_uq" })),
    },
    {
      name: "addCheck",
      via: () =>
        record(() => table("accounts").addCheck({ expr: (c) => c("balance").ge(0), name: "accounts_balance_chk" })),
      flat: () =>
        record(() => addCheck("accounts", { expr: (c) => c("balance").ge(0), name: "accounts_balance_chk" })),
    },
    {
      name: "dropConstraint",
      via: () => record(() => table("accounts").dropConstraint({ name: "accounts_legacy_chk", type: "check" })),
      flat: () => record(() => dropConstraint("accounts", { name: "accounts_legacy_chk", type: "check" })),
    },
    {
      name: "createIndex",
      via: () =>
        record(() =>
          table("accounts").createIndex({
            columns: ["email"],
            name: "accounts_active_email_idx",
            unique: true,
            where: (c) => c("active").isTrue(),
          }),
        ),
      flat: () =>
        record(() =>
          createIndex("accounts", {
            columns: ["email"],
            name: "accounts_active_email_idx",
            unique: true,
            where: (c) => c("active").isTrue(),
          }),
        ),
    },
    {
      name: "dropIndex (table stamped)",
      via: () => record(() => table("users").dropIndex("ix_email")),
      // The facade STAMPS the table — the flat equivalent supplies { table }.
      flat: () => record(() => dropIndex("ix_email", { table: "users" })),
    },
    {
      name: "insert",
      via: () =>
        record(() => table("status_codes").insert({ rows: [{ code: 200, label: "ok" }] })),
      flat: () => record(() => insert("status_codes", { rows: [{ code: 200, label: "ok" }] })),
    },
    {
      name: "update",
      via: () =>
        record(() =>
          table("status_codes").update({
            set: { norm: (c) => c.fn.lower(c("label")) },
            where: (c) => c("code").gt(0),
          }),
        ),
      flat: () =>
        record(() =>
          update("status_codes", {
            set: { norm: (c) => c.fn.lower(c("label")) },
            where: (c) => c("code").gt(0),
          }),
        ),
    },
    {
      name: "del",
      via: () => record(() => table("status_codes").del({ where: (c) => c("code").lt(0), limit: 10 })),
      flat: () => record(() => del("status_codes", { where: (c) => c("code").lt(0), limit: 10 })),
    },
    {
      name: "backfill",
      via: () =>
        record(() =>
          table("status_codes").backfill({
            set: { full: (c) => c.fn.concatWs(" ", c("label")) },
            cursorColumn: "code",
            batchSize: 500,
            name: "fluent_backfill",
          }),
        ),
      flat: () =>
        record(() =>
          backfill("status_codes", {
            set: { full: (c) => c.fn.concatWs(" ", c("label")) },
            cursorColumn: "code",
            batchSize: 500,
            name: "fluent_backfill",
          }),
        ),
    },
  ];

  for (const { name, via, flat } of cases) {
    assert.deepEqual(
      normalizeOps(via()),
      normalizeOps(flat()),
      `facade ≡ flat must record the identical op for "${name}"`,
    );
  }
});

// ───────────────────────────────────────────────────────────────────────────
// (B) full-migration parity against a committed golden. Re-author fluent_ddl's
// up() entirely through table() and assert it lowers to the byte-identical
// committed .ir.json — proving the facade is pure sugar over the whole surface.
// ───────────────────────────────────────────────────────────────────────────

test("(B) a table()-authored migration lowers to the byte-identical fluent_ddl golden", async () => {
  const ops = record(() => {
    table("accounts").create({
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
      label: t.string(),
      hits: t.int().notNull().default(0),
      big_hits: t.bigInt(),
      ratio: t.float(),
      secret: t.encrypted({ of: t.text() }),
    });
    table("memberships").create(
      { account_id: t.uuid().notNull(), team: t.text().notNull() },
      (b) => {
        b.primaryKey(["account_id", "team"]);
        b.unique(["team"], { name: "memberships_team_uq" });
        b.index(["account_id"], { name: "memberships_account_idx" });
        b.check((c) => c("team").isNotNull(), { name: "memberships_team_chk" });
        b.foreignKey({
          columns: ["account_id"],
          references: { table: "accounts", columns: ["id"] },
          name: "memberships_account_fk",
        });
      },
    );
    table("accounts").addColumn("status", t.text().notNull().default("new"));
    table("memberships").addForeignKey({
      columns: ["team"],
      references: { table: "teams", columns: ["name"] },
      name: "memberships_team_fk",
    });
    table("accounts").addUnique({ columns: ["external_id"], name: "accounts_external_uq" });
    table("accounts").addCheck({ expr: (c) => c("balance").ge(0), name: "accounts_balance_chk" });
    table("accounts").dropConstraint({ name: "accounts_legacy_chk", type: "check" });
    table("accounts").alterColumn("balance", { type: t.numeric(14, 2) });
    table("accounts").alterColumn("profile", { nullable: false });
    table("accounts").renameColumn("label", "display_label", t.text());
    table("accounts").createIndex({
      columns: ["email"],
      name: "accounts_active_email_idx",
      unique: true,
      where: (c) => c("active").isTrue(),
    });
    // The two trailing batchAlterTable ops in the golden are a SQLite-rebuild
    // grouping, not a per-table facade affordance — author them as the eager
    // equivalents (addColumn + alterColumn), which record the same two ops.
    table("accounts").addColumn("nickname", t.text());
    table("accounts").alterColumn("nickname", { nullable: false });
  });
  const g = await golden("fluent_ddl");
  assert.deepEqual(normalizeOps(ops), normalizeOps(g.ops));
});

test("(B) a table()-authored DML migration lowers to the byte-identical fluent_dml golden", async () => {
  const sc = table("status_codes");
  const ops = record(() => {
    sc.insert({
      rows: [
        { code: 200, label: "ok" },
        { code: 404, label: "not found" },
      ],
    });
    sc.update({
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
    sc.del({
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
    sc.backfill({
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

// ───────────────────────────────────────────────────────────────────────────
// EAGER: a handle method with NO further chaining records the op (no terminal).
// ───────────────────────────────────────────────────────────────────────────

test("EAGER: a single handle method call records immediately — no terminal", () => {
  const ops = record(() => {
    table("users").addColumn("age", t.int());
  });
  assert.equal(ops.length, 1, "one method call records exactly one op (no dangling builder)");
  assert.equal(ops[0].op, "addColumn");
  assert.equal(ops[0].table, "users");
  assert.equal(ops[0].column, "age");
});

test("EAGER: a reused handle records one op per call, in order", () => {
  const ops = record(() => {
    const u = table("users");
    u.addColumn("a", t.int());
    u.dropColumn("b");
    u.addColumn("c", t.text());
  });
  assert.deepEqual(
    ops.map((o) => [o.op, o.column]),
    [
      ["addColumn", "a"],
      ["dropColumn", "b"],
      ["addColumn", "c"],
    ],
  );
});

// ───────────────────────────────────────────────────────────────────────────
// SCHEMA PROPAGATION + PRECEDENCE.
// ───────────────────────────────────────────────────────────────────────────

test("SCHEMA: the table() default propagates onto every recorded op", () => {
  const ops = record(() => {
    const u = table("users", { schema: "app2" });
    u.addColumn("a", t.int());
    u.dropColumn("b");
    u.createIndex({ columns: ["a"], name: "ix_a" });
    u.dropIndex("ix_b");
    u.insert({ rows: [{ a: 1 }] });
    u.update({ set: { a: (c) => c("a") } });
    u.del({ where: (c) => c("a").gt(0) });
    u.backfill({ set: { a: (c) => c("a") } });
  });
  for (const op of ops) {
    assert.equal(op.schema, "app2", `op ${op.op} must carry the table default schema`);
  }
});

test("SCHEMA: a per-method schema OVERRIDES the table default", () => {
  const ops = record(() => {
    const u = table("users", { schema: "app2" });
    u.addColumn("a", t.int(), { schema: "other" }); // override
    u.createIndex({ columns: ["a"], name: "ix_a", schema: "idx_schema" }); // override on spec
    u.insert({ rows: [{ a: 1 }], schema: "dml_schema" }); // override on args
  });
  assert.equal(ops[0].schema, "other");
  assert.equal(ops[1].schema, "idx_schema");
  assert.equal(ops[2].schema, "dml_schema");
});

test("SCHEMA: a per-method opts bag WITHOUT a schema key keeps the table default", () => {
  const ops = record(() => {
    const u = table("users", { schema: "app2" });
    u.addColumn("a", t.int(), { ifNotExists: true }); // guard only, no schema key
    u.dropColumn("b", { ifExists: true });
  });
  assert.equal(ops[0].schema, "app2", "guard-only opts must not wipe the table default");
  assert.equal(ops[1].schema, "app2");
});

test("SCHEMA: table() with no schema records ops with NO schema key (identical to flat)", () => {
  const facadeOps = record(() => table("users").addColumn("a", t.int()));
  const flatOps = record(() => addColumn("users", "a", t.int()));
  assert.ok(!("schema" in facadeOps[0]), "no schema default ⇒ schema key omitted");
  assert.deepEqual(facadeOps, flatOps);
});

// ───────────────────────────────────────────────────────────────────────────
// GUARD PASS-THROUGH.
// ───────────────────────────────────────────────────────────────────────────

test("GUARD: ifNotExists / ifExists pass through per-method to existenceGuard", () => {
  const ops = record(() => {
    const u = table("users");
    u.addColumn("a", t.int(), { ifNotExists: true });
    u.dropColumn("b", { ifExists: true });
    u.create({ id: t.id() }, undefined, { ifNotExists: true });
    u.drop({ ifExists: true });
    u.createIndex({ columns: ["a"], name: "ix_a", ifNotExists: true });
    u.dropIndex("ix_b", { ifExists: true });
  });
  assert.equal(ops[0].existenceGuard, "ifNotExists");
  assert.equal(ops[1].existenceGuard, "ifExists");
  assert.equal(ops[2].existenceGuard, "ifNotExists");
  assert.equal(ops[3].existenceGuard, "ifExists");
  assert.equal(ops[4].existenceGuard, "ifNotExists");
  assert.equal(ops[5].existenceGuard, "ifExists");
});

test("GUARD: no guard option ⇒ existenceGuard omitted (identical to flat)", () => {
  const facadeOps = record(() => table("users").addColumn("a", t.int()));
  assert.ok(!("existenceGuard" in facadeOps[0]), "absent guard ⇒ existenceGuard key omitted");
});
