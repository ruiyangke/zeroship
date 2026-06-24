// Parity guard: the npm `@zeroship/migrate` DSL records the SAME ops the engine's
// embedded recorder (`crates/zeroship-migrate-js/src/migrate_ops.js`) committed
// into the golden corpus. The npm `ops.ts` and the V8-embedded `migrate_ops.js`
// are two implementations of the same locked surface (§3.2/§3.3.1); this test
// re-authors a golden fixture's `up()` through the npm DSL and asserts the
// recorded op list equals the committed golden `.ir.json`'s `ops` — so the two
// implementations cannot silently diverge on the frozen wire shape.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  addColumn,
  addCheck,
  addForeignKey,
  addUnique,
  alterColumn,
  backfill,
  batchAlterTable,
  createIndex,
  createTable,
  del,
  dropConstraint,
  insert,
  renameColumn,
  t,
  update,
} from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixturesDir = resolve(here, "../../../crates/zeroship-migrate-js/tests/op_fixtures");

async function golden(stem: string): Promise<any> {
  return JSON.parse(await readFile(resolve(fixturesDir, `${stem}.ir.json`), "utf8"));
}

/** Normalize a `createTable` op for parity comparison: the committed golden is the
 *  RUST RE-SERIALIZATION of the typed IR, which fills `constraints`/`indexes` with
 *  serde defaults (`[]`) on serialize; the npm DSL OMITS empty arrays (the
 *  cross-impl-determinism `skip_serializing_if`-style image). Both deserialize to
 *  the same typed op (absent == empty), so we drop empty `constraints`/`indexes`
 *  on both sides before comparing — the wire shape is otherwise identical. */
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

test("fluent_ddl npm-recorded ops equal the committed golden", async () => {
  const ops = record(() => {
    createTable("accounts", {
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
    createTable(
      "memberships",
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
    addColumn("accounts", "status", t.text().notNull().default("new"));
    addForeignKey("memberships", {
      columns: ["team"],
      references: { table: "teams", columns: ["name"] },
      name: "memberships_team_fk",
    });
    addUnique("accounts", { columns: ["external_id"], name: "accounts_external_uq" });
    addCheck("accounts", { expr: (c) => c("balance").ge(0), name: "accounts_balance_chk" });
    dropConstraint("accounts", { name: "accounts_legacy_chk", type: "check" });
    alterColumn("accounts", "balance", { type: t.numeric(14, 2) });
    alterColumn("accounts", "profile", { nullable: false });
    renameColumn("accounts", "label", "display_label", t.text());
    createIndex("accounts", {
      columns: ["email"],
      name: "accounts_active_email_idx",
      unique: true,
      where: (c) => c("active").isTrue(),
    });
    batchAlterTable("accounts", (b) => {
      b.addColumn("nickname", t.text());
      b.alterColumn("nickname", { nullable: false });
    });
  });
  const g = await golden("fluent_ddl");
  assert.deepEqual(normalizeOps(ops), normalizeOps(g.ops));
});

test("fluent_dml npm-recorded ops equal the committed golden", async () => {
  const ops = record(() => {
    insert("status_codes", {
      rows: [
        { code: 200, label: "ok" },
        { code: 404, label: "not found" },
      ],
    });
    update("status_codes", {
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
    del("status_codes", {
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
    backfill("status_codes", {
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
