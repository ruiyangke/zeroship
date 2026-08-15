// The SQL `plan` shows is the SQL that produces what `apply` produces.
//
// `plan` exists to be REVIEWED. An operator reads its output, approves it, and
// expects the deploy to do that. If the render path and the apply path ever drift,
// the reviewed statement and the executed one stop being the same thing, and
// nothing about a successful deploy would reveal it.
//
// `sql-preview-parity.test.ts` pins the render path against hand-written SQL
// expectations, and says in its own header what it does NOT cover:
//
//   "Nothing about apply-time behaviour. `previewSql` is the DB-free render path;
//    it opens no connection and executes nothing."
//
// This is that half. It does not compare SQL TEXT, because apply is not obliged to
// emit byte-identical statements -- it compares the RESULTING SCHEMA, which is what
// an operator is actually approving when they read a plan:
//
//   1. `apply` into one namespace;
//   2. `plan --json` for a second namespace, and execute exactly that SQL by hand;
//   3. compare columns, indexes and constraints from the catalog.
//
// Each namespace gets its own plan, so each SQL text is correct for the schema it
// runs in, and the index definitions are normalised for the namespace name before
// comparison.
//
// The migration is deliberately not minimal. A `CREATE TABLE` alone would pass on a
// renderer that dropped every constraint, so it carries numeric precision/scale,
// timestamptz, a text array, jsonb, a named UNIQUE, a named CHECK, and a separate
// CREATE INDEX -- the facets a preview is most likely to render differently from
// what it executes.
//
// THE CONTROL IS A TEST, NOT A COMMENT. "The two schemas matched" is also what a
// comparison prints when it compares nothing useful. The second test replays only
// part of the previewed SQL and REQUIRES the comparison to notice, so a
// same-shaped-always bug in the comparison fails the suite instead of reporting
// parity forever.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_plan_parity";
const TABLE = "pp_users";

function project(): string {
  const work = mkdtempSync(join(HERE, "planparity-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  schema() {
    table("${TABLE}").create({
      columns: {
        id: t.int().notNull(),
        email: t.text().notNull(),
        rank: t.int(),
        total: t.numeric({ precision: 12, scale: 2 }),
        created: t.timestamp().notNull(),
        tags: t.textArray(),
        meta: t.json(),
      },
      primaryKey: ["id"],
      uniques: [{ name: "uq_${TABLE}_email", columns: ["email"] }],
      checks: [{ name: "ck_${TABLE}_rank", expr: (col) => col("rank").ge(0) }],
    });
    table("${TABLE}").index("ix_${TABLE}_rank").add({ on: [{ column: "rank" }] });
  },
};
`,
  );
  return work;
}

function cli(
  work: string,
  namespace: string,
  args: readonly string[],
): { code: number | null; out: string; err: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...args,
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", namespace,
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return { code: result.status, out: (result.stdout ?? "").trim(), err: (result.stderr ?? "").trim() };
}

/** The previewed SQL for one namespace, concatenated in plan order. */
function previewedSql(work: string, namespace: string): string {
  const planned = cli(work, namespace, ["plan", "--json"]);
  assert.equal(planned.code, 0, `plan must succeed; ${planned.err}`);
  const parsed = JSON.parse(planned.out) as {
    pending: Array<{ sql: string }>;
    blocked: Array<unknown>;
  };
  assert.ok(parsed.pending.length > 0, "plan must report pending work to preview");
  assert.deepEqual(parsed.blocked, [], "ordinary runnable work has no blocked listing");
  return parsed.pending.map((entry) => entry.sql).join("\n");
}

/** Columns, indexes and constraints, with the namespace name normalised out of
 *  index definitions so two namespaces are comparable. */
async function shapeOf(
  client: import("pg").Client,
  namespace: string,
): Promise<{ cols: unknown[]; idx: unknown[]; cons: unknown[] }> {
  const cols = (
    await client.query(
      `SELECT column_name, data_type, is_nullable, numeric_precision, numeric_scale
         FROM information_schema.columns WHERE table_schema = $1
        ORDER BY table_name, column_name`,
      [namespace],
    )
  ).rows;
  const idx = (
    await client.query(
      `SELECT indexname, regexp_replace(indexdef, $2, 'NS', 'g') AS def
         FROM pg_indexes WHERE schemaname = $1 ORDER BY indexname`,
      [namespace, namespace],
    )
  ).rows;
  const cons = (
    await client.query(
      `SELECT c.conname, pg_get_constraintdef(c.oid) AS def
         FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace
        WHERE n.nspname = $1 ORDER BY c.conname`,
      [namespace],
    )
  ).rows;
  return { cols, idx, cons };
}

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

test("replaying the previewed SQL builds exactly what apply builds", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const applied = uniqueNamespace("planparity_apply");
  const manual = uniqueNamespace("planparity_manual");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${applied}"`);
    await client.query(`CREATE SCHEMA "${manual}"`);

    const ran = cli(work, applied, ["apply", "--approve"]);
    assert.equal(ran.code, 0, `apply must succeed; ${ran.err}`);

    // The preview for the OTHER namespace, executed verbatim.
    await client.query(previewedSql(work, manual));

    const fromApply = await shapeOf(client, applied);
    const fromPlan = await shapeOf(client, manual);
    assert.deepEqual(
      fromPlan,
      fromApply,
      "the schema built by the previewed SQL must be the schema apply builds, or " +
        "the statement an operator reviewed is not the statement that ran",
    );
  } finally {
    for (const namespace of [applied, manual]) {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
           DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
        )
        .catch(() => {});
    }
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("CONTROL: the comparison notices when the replay is incomplete", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const applied = uniqueNamespace("planparity_capply");
  const manual = uniqueNamespace("planparity_cmanual");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${applied}"`);
    await client.query(`CREATE SCHEMA "${manual}"`);

    const ran = cli(work, applied, ["apply", "--approve"]);
    assert.equal(ran.code, 0, `apply must succeed; ${ran.err}`);

    // Replay the CREATE TABLE but not the CREATE INDEX. The two schemas now
    // genuinely differ, and `shapeOf` must say so -- otherwise the test above is
    // comparing something that cannot disagree, and its green means nothing.
    const sql = previewedSql(work, manual);
    const withoutIndex = sql
      .split(";")
      .filter((statement) => !/CREATE\s+(UNIQUE\s+)?INDEX/i.test(statement))
      .join(";");
    await client.query(withoutIndex);

    const fromApply = await shapeOf(client, applied);
    const fromPartial = await shapeOf(client, manual);
    assert.notDeepEqual(
      fromPartial,
      fromApply,
      "dropping a statement from the replay must change the compared shape; if it " +
        "does not, the parity assertion above cannot detect a divergence either",
    );
  } finally {
    for (const namespace of [applied, manual]) {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
           DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
        )
        .catch(() => {});
    }
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
