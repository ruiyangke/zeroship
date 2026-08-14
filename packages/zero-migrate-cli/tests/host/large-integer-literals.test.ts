// An integer literal survives the apply boundary exactly, up to the engine's 2^53.
//
// The IR's contract is `IrScalar::Int(i64)`, exact while `|v| < 2^53`, and its own
// error text for anything larger says so. But the envelope reaches the addon two
// different ways, and only one of them used to preserve an integer:
//
//   lint / validate   `JSON.stringify(envelope)` -- the text carries 4294967296
//   apply             the envelope crosses as a JS VALUE through napi, where a
//                     number above `u32::MAX` becomes an f64 and arrives as
//                     4294967296.0
//
// So `apply` rejected it as "fractional or exponential" -- a rule the value does
// not break -- while `lint` passed the same migration. Green in CI, dead at
// deploy, with a message naming neither the value nor the column.
//
// THE TRIGGER IS ORDINARY. A millisecond timestamp is ~1.78e12; snowflake ids and
// large counters are similar. Anything above 4294967295 was affected, which is a
// low bar for real data.
//
// The boundary was sharp at 2^32, so the cases below straddle it deliberately
// rather than picking one large number: `u32::MAX` passed before this fix and must
// keep passing, and `u32::MAX + 1` is the first value that did not.
//
// BOTH VERBS ARE ASSERTED ON THE SAME MIGRATION. The defect was not that apply was
// wrong in isolation -- it was that the two disagreed, so a test of either alone
// would have missed it.
//
// GATES: the literal arms run everywhere (SQLite exercises the same crossing); the
// partition-bound arm needs `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_bigint";
const TABLE = "bigint_rows";

/** Straddles the old `u32::MAX` cliff, plus a real millisecond timestamp and the
 *  largest value the engine's own contract allows. */
const VALUES: ReadonlyArray<readonly [string, string]> = [
  ["u32::MAX (passed before)", "4294967295"],
  ["u32::MAX + 1 (the first failure)", "4294967296"],
  ["a millisecond timestamp", "1786707868430"],
  ["2^53 - 1, the engine's own limit", "9007199254740991"],
  // The restore keys on |v|, so a future narrowing to a positive-only check
  // would break these while every case above kept passing.
  ["negative, just past u32::MAX", "-4294967296"],
  ["negative millisecond timestamp", "-1786707868430"],
  ["-(2^53 - 1), the negative limit", "-9007199254740991"],
];

function project(value: string): string {
  const work = mkdtempSync(join(HERE, "bigint-"));
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
  up() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), ts: t.bigInt() },
      primaryKey: ["id"],
    });
    table("${TABLE}").insert({ rows: [{ id: 1, ts: ${value} }] });
  },
};
`,
  );
  return work;
}

function run(
  work: string,
  verb: string,
  extra: readonly string[] = [],
): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, verb, ...extra,
      "--dir", join(work, "migrations"),
      "--database-url", `sqlite:${join(work, "app.db")}`,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("a large integer literal applies, and lint agrees, on both sides of 2^32", () => {
  for (const [label, value] of VALUES) {
    const work = project(value);
    try {
      const linted = run(work, "lint");
      const applied = run(work, "apply", ["--approve"]);

      assert.equal(linted.code, 0, `${label}: lint must accept ${value}; ${linted.text}`);
      assert.equal(
        applied.code,
        0,
        `${label}: apply must accept the same migration lint just passed -- a value ` +
          `below the engine's 2^53 contract must survive the envelope crossing; ${applied.text}`,
      );

      // The value must arrive EXACTLY. A float round-trip would land near it
      // rather than on it, which an exit code cannot see.
      const db = new DatabaseSync(join(work, "app.db"), { readOnly: true });
      const row = db.prepare(`SELECT ts FROM ${TABLE} WHERE id = 1`).get() as { ts: unknown };
      db.close();
      assert.equal(
        String(row.ts),
        value,
        `${label}: the stored value must be exactly what was authored`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("an integer at or beyond 2^53 is still refused, and lint agrees", () => {
  // The engine's contract is |v| < 2^53, and `int64("…")` is the documented
  // carrier past it. Widening the crossing must not widen THAT -- an inexact
  // JS number should not start applying silently.
  const work = project("9007199254740993");
  try {
    const applied = run(work, "apply", ["--approve"]);
    assert.equal(applied.code, 1, `beyond 2^53 must still be refused; ${applied.text}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

/** The same crossing, reached through a DIFFERENT IR node shape.
 *
 *  A partition bound is not an `IrScalar` -- it is a bound node carrying its own
 *  `{ kind: "int", value }` -- so it exercises the restore through a separate part
 *  of the envelope tree. It is also the most natural home for a large integer in
 *  real schemas: range-partitioning by millisecond timestamp puts values around
 *  1.78e12 directly into the DDL.
 *
 *  The assertion reads the bound back out of `pg_class.relpartbound` rather than
 *  trusting the exit code, because a widened value would still produce a valid
 *  partition -- just one whose boundary is in the wrong place, which is a data
 *  routing bug rather than an error. */
test("a large integer partition bound reaches the catalog exactly", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const { pgUrl } = await import("./live-db.js");
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();

  const FROM = "1786707868430";
  const TO = "1786794268430";
  const namespace = `bigpart_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
  const work = mkdtempSync(join(HERE, "bigpart-"));
  try {
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
    writeFileSync(
      join(work, "registry.json"),
      JSON.stringify({ part_t: OWNER_APP, part_t_win: OWNER_APP }),
    );
    writeFileSync(
      join(work, "migrations", "20260101000000_a.ts"),
      `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("part_t").create({
      columns: { id: t.int().notNull(), ms: t.bigInt().notNull() },
      primaryKey: ["id", "ms"],
      partitionBy: { range: ["ms"] },
    });
    table("part_t").partition("part_t_win").create({ from: [${FROM}], to: [${TO}] });
  },
};
`,
    );
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = spawnSync(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
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
    assert.equal(
      applied.status,
      0,
      `the partitioned table must apply; ${`${applied.stdout}\n${applied.stderr}`.trim()}`,
    );

    const { rows } = await client.query(
      `SELECT pg_get_expr(c.relpartbound, c.oid) AS bound
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = 'part_t_win'`,
      [namespace],
    );
    const bound = String(rows[0]?.bound ?? "");
    assert.ok(bound.includes(FROM), `the lower bound must be exact; got ${bound}`);
    assert.ok(bound.includes(TO), `the upper bound must be exact; got ${bound}`);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
