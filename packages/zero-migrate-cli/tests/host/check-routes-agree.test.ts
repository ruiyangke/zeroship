// The three ways to author a CHECK constraint produce the same constraint, and
// all three stop at the same dialect boundary.
//
// A check can be written three ways, and nothing compared them end to end:
//
//   1. the standalone `check(name, expr)` helper, inside `create({ checks })`
//   2. a plain `{ name, expr }` object, inside the same `checks` array
//   3. `table(x).check(name).add({ expr })`, a separate op after the table exists
//
// `ops.test.ts` pins what each RECORDS at the DSL layer. That is not the same as
// them meaning one thing to a database: routes 1 and 2 land inside `CREATE TABLE`
// while route 3 is an `ALTER TABLE`, so they reach the renderer by different paths
// and could diverge without any DSL-level test noticing. `check` had no host
// coverage at all.
//
// The predicates are chosen so a route that took its own rendering path would be
// caught rather than merely suspected:
//
//   n >= 0                       the trivial case
//   kind IN ('a', "b'c", 'd')    a LIST, and one element carries an EMBEDDED QUOTE
//   floor IS NULL OR floor >= 0  NULL-tolerance, where CHECK semantics are subtle
//   NOT (kind = "arch'ived")     negation, plus a second embedded quote
//
// The two quoted literals matter most: a route that escaped differently would
// either produce a different predicate or fail to apply, and both show up here.
//
// THE ASSERTION IS EQUALITY BETWEEN ROUTES, not a hardcoded predicate string per
// route. What an author relies on is that the three spellings mean one thing;
// pinning three literals would still pass if all three drifted together into
// something the DSL never promised. Enforcement is asserted too -- identical
// predicate text is worth little if the constraint is not actually checked -- with
// a valid row proving the constraints are not simply rejecting everything.
//
// THE DIALECT BOUNDARY IS THE SECOND HALF. `dialects.md` makes two separate
// PG-only claims, one per authoring position:
//
//   | Table-level `checks`  | Yes | No | No |
//   | Add standalone check  | Yes | No | No |
//
// F627 is why this is measured rather than read: there, a PG-only feature passed
// `lint` and then failed at `apply` -- green CI, broken deploy, the one failure
// lint exists to prevent. Both rows hold here, on both verbs, for both routes.
// MySQL 8 and SQLite both support CHECK natively, so this is a real engine
// limitation rather than a database one, which is exactly the kind of restriction
// that goes stale silently.
//
// GATES: SQLite lint runs anywhere; the rest need `ZERO_MIGRATE_TEST_PG_URL` and
// `ZERO_MIGRATE_MYSQL_URL`.

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

const OWNER_APP = "app_ckroutes";
const ROUTES = ["c_helper", "c_object", "c_addop"] as const;

/** `[constraint, violating value of n / kind / floor]`. */
const VIOLATIONS: ReadonlyArray<readonly [string, string]> = [
  ["ck_nonneg", "(1, -1, 'a', 0)"],
  ["ck_kind_in", "(2, 0, 'zzz', 0)"],
  ["ck_floor", "(3, 0, 'a', -1)"],
  ["ck_not_arch", `(4, 0, 'arch''ived', 0)`],
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** All three routes in one migration, so they are rendered by the same run. */
const ALL_ROUTES = `import { table, t, check } from "zero-migrate";
export const name = "a";
const cols = {
  id: t.int().notNull(),
  n: t.int().notNull(),
  kind: t.text().notNull(),
  floor: t.int(),
};
const P = [
  ["ck_nonneg", (col) => col("n").ge(0)],
  // "arch'ived" is IN this list on purpose: it lets a row violate ck_not_arch
  // while still SATISFYING ck_kind_in, so each violating row below trips exactly
  // one constraint and the rejection can be attributed to it. Without that, the
  // ck_not_arch row also falls outside the list and PostgreSQL names whichever
  // constraint it reached first.
  ["ck_kind_in", (col) => col("kind").in(["a", "b'c", "d", "arch'ived"])],
  ["ck_floor", (col) => col("floor").isNull().or(col("floor").ge(0))],
  ["ck_not_arch", (col) => col("kind").eq("arch'ived").not()],
];
export default {
  up() {
    // 1: the standalone helper
    table("c_helper").create({
      columns: cols, primaryKey: ["id"],
      checks: P.map(([n, e]) => check(n, e)),
    });
    // 2: a plain object literal
    table("c_object").create({
      columns: cols, primaryKey: ["id"],
      checks: P.map(([name, expr]) => ({ name, expr })),
    });
    // 3: the separate add op, against an existing table
    table("c_addop").create({ columns: cols, primaryKey: ["id"] });
    for (const [n, e] of P) table("c_addop").check(n).add({ expr: e });
  },
};
`;

/** Route 1 alone -- the "Table-level `checks`" row of the dialect table. */
const CREATE_TIME_ONLY = `import { table, t, check } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("c_helper").create({
      columns: { id: t.int().notNull(), n: t.int().notNull() },
      primaryKey: ["id"],
      checks: [check("ck_nonneg", (col) => col("n").ge(0))],
    });
  },
};
`;

/** Route 3 alone -- the "Add standalone check" row, which is gated separately. */
const ADD_OP_ONLY = `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("c_addop").create({
      columns: { id: t.int().notNull(), n: t.int().notNull() },
      primaryKey: ["id"],
    });
    table("c_addop").check("ck_nonneg").add({ expr: (col) => col("n").ge(0) });
  },
};
`;

function project(source: string): string {
  const work = mkdtempSync(join(HERE, "ckroutes-"));
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
    JSON.stringify(Object.fromEntries(ROUTES.map((table) => [table, OWNER_APP]))),
  );
  writeFileSync(join(work, "migrations", "20260101000000_a.ts"), source);
  return work;
}

function run(
  work: string,
  verb: string,
  extra: readonly string[],
  databaseUrl?: string,
  namespace?: string,
): { code: number | null; text: string } {
  const args = [
    "--import", "tsx", CLI_BIN, verb, ...extra,
    "--dir", join(work, "migrations"),
    "--policy", join(work, "policy.toml"),
    "--registry", join(work, "registry.json"),
    "--owner-app", OWNER_APP,
  ];
  if (databaseUrl) args.push("--database-url", databaseUrl);
  if (namespace) args.push("--schema", namespace);
  const result = spawnSync(process.execPath, args, {
    cwd: work,
    encoding: "utf8",
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("all three authoring routes render the identical check predicate", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("ckroutes");
  const work = project(ALL_ROUTES);
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = run(work, "apply", ["--approve"], pgUrl(), namespace);
    assert.equal(applied.code, 0, `all three routes must apply; ${applied.text}`);

    const { rows } = await client.query(
      `SELECT c.conname, rel.relname, pg_get_constraintdef(c.oid) AS def
         FROM pg_constraint c
         JOIN pg_class rel ON rel.oid = c.conrelid
         JOIN pg_namespace n ON n.oid = rel.relnamespace
        WHERE n.nspname = $1 AND c.contype = 'c'
        ORDER BY c.conname, rel.relname`,
      [namespace],
    );

    const byConstraint = new Map<string, Map<string, string>>();
    for (const row of rows as Array<{ conname: string; relname: string; def: string }>) {
      if (!byConstraint.has(row.conname)) byConstraint.set(row.conname, new Map());
      byConstraint.get(row.conname)!.set(row.relname, row.def);
    }

    assert.equal(
      byConstraint.size,
      VIOLATIONS.length,
      `every predicate must survive on every route; got ${JSON.stringify([...byConstraint.keys()])}`,
    );
    for (const [conname, perTable] of byConstraint) {
      assert.deepEqual(
        [...perTable.keys()].sort(),
        [...ROUTES].sort(),
        `${conname}: must exist on all three routes`,
      );
      const distinct = new Set(perTable.values());
      assert.equal(
        distinct.size,
        1,
        `${conname}: the three routes must mean ONE thing -- routes 1 and 2 render ` +
          `inside CREATE TABLE and route 3 as an ALTER, so a divergence here is a ` +
          `real difference in what the author gets; got ${JSON.stringify([...perTable])}`,
      );
    }
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

test("and all three enforce it against real rows", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("ckenforce");
  const work = project(ALL_ROUTES);
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    assert.equal(run(work, "apply", ["--approve"], pgUrl(), namespace).code, 0);

    for (const [conname, values] of VIOLATIONS) {
      for (const table of ROUTES) {
        await assert.rejects(
          () =>
            client.query(
              `INSERT INTO "${namespace}"."${table}" (id, n, kind, floor) VALUES ${values}`,
            ),
          (error: { message: string }) =>
            new RegExp(conname).test(error.message),
          `${table}: the row violating ${conname} must be rejected BY THAT ` +
            `constraint -- identical predicate text is worth nothing if the ` +
            `constraint is not actually enforced`,
        );
      }
    }

    // Without this the test above passes on three tables that reject everything.
    for (const table of ROUTES) {
      await client.query(
        `INSERT INTO "${namespace}"."${table}" (id, n, kind, floor) VALUES (9, 5, 'a', NULL)`,
      );
    }
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

test("lint refuses checks on SQLite for both documented rows, and apply agrees", () => {
  for (const [row, source] of [
    ["Table-level `checks`", CREATE_TIME_ONLY],
    ["Add standalone check", ADD_OP_ONLY],
  ] as const) {
    const work = project(source);
    try {
      const linted = run(work, "lint", ["--dialect", "sqlite"]);
      assert.equal(
        linted.code,
        1,
        `${row}: lint must refuse it OFFLINE. F627 is the precedent: a PG-only ` +
          `feature that passes lint and fails at apply is green CI and a broken ` +
          `deploy; ${linted.text}`,
      );
      const applied = run(work, "apply", ["--approve"], `sqlite:${join(work, "app.db")}`);
      assert.equal(applied.code, 1, `${row}: apply must agree; ${applied.text}`);
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("lint refuses checks on MySQL for both documented rows, and apply agrees", async (ctx) => {
  const mysqlUrl = process.env.ZERO_MIGRATE_MYSQL_URL;
  if (!mysqlUrl) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const admin = await driver.createConnection({ uri: String(mysqlUrl) });
  const base = String(mysqlUrl).replace(/\/[^/]*$/, "");
  try {
    for (const [row, source] of [
      ["Table-level `checks`", CREATE_TIME_ONLY],
      ["Add standalone check", ADD_OP_ONLY],
    ] as const) {
      const work = project(source);
      const namespace = uniqueNamespace("ck_my");
      try {
        const linted = run(work, "lint", ["--dialect", "mysql"]);
        assert.equal(
          linted.code,
          1,
          `${row}: lint must refuse it. MySQL 8 supports CHECK natively, so this ` +
            `is an ENGINE limitation, which is the kind that goes stale quietly; ` +
            `${linted.text}`,
        );
        await admin.query(`CREATE DATABASE \`${namespace}\``);
        const applied = run(work, "apply", ["--approve"], `${base}/${namespace}`, namespace);
        assert.equal(applied.code, 1, `${row}: apply must agree; ${applied.text}`);
      } finally {
        await admin.query(`DROP DATABASE IF EXISTS \`${namespace}\``).catch(() => {});
        await admin.query(`DROP DATABASE IF EXISTS \`${namespace}_migrations\``).catch(() => {});
        rmSync(work, { recursive: true, force: true });
      }
    }
  } finally {
    await admin.end().catch(() => {});
  }
});

/** Without this, four refusals are equally consistent with two malformed sources. */
test("CONTROL: both rows are accepted on PostgreSQL", () => {
  for (const [row, source] of [
    ["Table-level `checks`", CREATE_TIME_ONLY],
    ["Add standalone check", ADD_OP_ONLY],
  ] as const) {
    const work = project(source);
    try {
      const linted = run(work, "lint", ["--dialect", "postgres"]);
      assert.equal(
        linted.code,
        0,
        `${row}: PostgreSQL must ACCEPT what the others refuse, or the refusals ` +
          `say nothing about dialect gating; ${linted.text}`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});
