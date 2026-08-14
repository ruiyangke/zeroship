// `t.vector` and `t.geoPoint` emit a derived index only where the target can build it.
//
// Neither type's index is authored. Both are derived from the column type, to model
// what the PostgreSQL data plane creates (`USING ivfflat`, `USING gist`) so a
// runtime-created index does not phantom-drop out of the desired snapshot.
//
// On MySQL both types land as `blob`, and a plain index over one is refused
// outright:
//
//   t.vector    BLOB/TEXT column 'payload' used in key specification without a
//               key length
//   t.geoPoint  All parts of a SPATIAL index must be NOT NULL
//
// Both refusals arrive at APPLY. `lint --dialect mysql` reported the same
// migrations clean, so the cost was a green CI followed by a broken deploy -- the
// exact false green `lint-dialect-verdicts.test.ts` names as the failure that
// matters. Three of the four cases in that class behaved this way; only
// `geoPoint().notNull()` applied, because MySQL's SPATIAL requirement happens to be
// satisfied by NOT NULL while the vector BLOB rule is not satisfiable at all.
//
// So the index is no longer emitted on MySQL. The COLUMN still is: dropping the
// index keeps the declaration usable, and what is lost is an index the author never
// wrote and MySQL could never have had.
//
// SQLITE KEEPS ITS INDEX, and that asymmetry is the point of this file rather than
// an oversight. `blob` is indexable on SQLite; both cases apply cleanly and the
// index is really in `sqlite_master`. "Emit only where buildable" is a claim about
// each target separately, not a shorthand for "PostgreSQL only", so the SQLite arm
// asserts the index is PRESENT. A change that skipped every non-PostgreSQL target
// would pass every MySQL assertion here and fail that one.
//
// PostgreSQL keeps its native access methods, asserted from `pg_am` rather than
// from the index merely existing -- a btree over a vector column would satisfy
// "an index exists" while being the wrong object entirely.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL`, `ZERO_MIGRATE_MYSQL_URL`. SQLite always runs.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
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
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_ann_index";

/** Both types, and for geoPoint both nullabilities: `notNull` rescued the MySQL
 *  geoPoint case before this change and must keep working after it. */
const CASES = [
  { what: "vector", column: `t.vector({ dimensions: 3, metric: "cosine" })`, table: "ann_vec" },
  { what: "geoPoint", column: "t.geoPoint()", table: "ann_geo" },
  { what: "geoPoint notNull", column: "t.geoPoint().notNull()", table: "ann_geo2" },
] as const;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(column: string, table: string): string {
  const work = mkdtempSync(join(HERE, "annidx-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [table]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_base.ts"),
    `import { table, t } from "zero-migrate";
export const name = "base";
export default {
  up() {
    table("${table}").create({
      columns: { id: t.int().notNull(), payload: ${column} },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  return work;
}

function run(
  work: string,
  args: readonly string[],
): Promise<{ code: number | null; text: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, ...args,
        "--dir", join(work, "migrations"),
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim(),
      }),
    );
  });
}

test("MySQL applies both types, and lint agrees", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset");
    return;
  }
  const driver = (await import("mysql2/promise")).default;
  const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
  const admin = await driver.createConnection({ uri: String(MYSQL_URL) });
  try {
    for (const testCase of CASES) {
      const work = project(testCase.column, testCase.table);
      const namespace = uniqueNamespace("annidx_my");
      try {
        // lint first: its verdict is the one CI acts on, and it must not disagree
        // with what apply does two lines below.
        const linted = await run(work, ["lint", "--dialect", "mysql"]);
        assert.equal(linted.code, 0, `${testCase.what}: lint must pass; ${linted.text}`);

        await admin.query(`CREATE DATABASE \`${namespace}\``);
        const applied = await run(work, [
          "apply", "--approve",
          "--database-url", `${base}/${namespace}`,
          "--schema", namespace,
        ]);
        assert.equal(
          applied.code,
          0,
          `${testCase.what}: apply must agree with lint, not fail on an index the ` +
            `author never asked for; ${applied.text}`,
        );

        // The column survives -- only the underivable index is dropped.
        const [columns] = await admin.query(
          `SELECT COLUMN_NAME AS c FROM information_schema.COLUMNS
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY 1`,
          [namespace, testCase.table],
        );
        assert.deepEqual(
          (columns as Array<{ c: string }>).map((row) => row.c),
          ["id", "payload"],
          `${testCase.what}: the column must still be created`,
        );

        // And no index over it, since MySQL cannot build one.
        const [indexes] = await admin.query(
          `SELECT INDEX_NAME AS i FROM information_schema.STATISTICS
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = 'payload'`,
          [namespace, testCase.table],
        );
        assert.deepEqual(
          (indexes as Array<{ i: string }>).map((row) => row.i),
          [],
          `${testCase.what}: MySQL must carry no derived index over the column`,
        );
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

test("SQLite keeps the derived index, because it can build one", async () => {
  for (const testCase of CASES) {
    const work = project(testCase.column, testCase.table);
    try {
      const applied = await run(work, [
        "apply", "--approve",
        "--database-url", `sqlite:${join(work, "app.db")}`,
      ]);
      assert.equal(applied.code, 0, `${testCase.what}: SQLite must apply; ${applied.text}`);

      const db = new DatabaseSync(join(work, "app.db"), { readOnly: true });
      const indexes = (
        db
          .prepare(`SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=?`)
          .all(testCase.table) as Array<{ name: string }>
      ).map((row) => row.name);
      db.close();

      // The asymmetry with MySQL is deliberate. A change that skipped the index on
      // every non-PostgreSQL target would satisfy every MySQL assertion above and
      // fail here, which is exactly what this arm is for.
      assert.deepEqual(
        indexes,
        [`${testCase.table}_payload_idx`],
        `${testCase.what}: SQLite indexes a blob happily, so the index must remain`,
      );
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  }
});

test("PostgreSQL keeps its native access methods", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  // pgvector/PostGIS may not be installed on the test server; the geoPoint and
  // vector column types need them, so this arm reports rather than pretends.
  const { rows: available } = await client.query(
    `SELECT name FROM pg_available_extensions WHERE name IN ('vector','postgis')`,
  );
  const have = new Set(available.map((row: { name: string }) => row.name));
  try {
    for (const testCase of CASES) {
      const needed = testCase.what.startsWith("vector") ? "vector" : "postgis";
      if (!have.has(needed)) {
        ctx.diagnostic(`skipping ${testCase.what}: extension ${needed} unavailable`);
        continue;
      }
      const namespace = uniqueNamespace("annidx_pg");
      const work = project(testCase.column, testCase.table);
      try {
        await client.query(`CREATE SCHEMA "${namespace}"`);
        const applied = await run(work, [
          "apply", "--approve",
          "--database-url", pgUrl(),
          "--schema", namespace,
        ]);
        assert.equal(applied.code, 0, `${testCase.what}: PG must apply; ${applied.text}`);

        // Read the ACCESS METHOD, not merely that an index exists: a btree over a
        // vector column would satisfy "an index exists" while being the wrong
        // object, which is the regression this arm has to catch.
        const { rows } = await client.query(
          `SELECT am.amname FROM pg_class i
             JOIN pg_index x ON x.indexrelid = i.oid
             JOIN pg_class t ON t.oid = x.indrelid
             JOIN pg_namespace n ON n.oid = t.relnamespace
             JOIN pg_am am ON am.oid = i.relam
            WHERE n.nspname = $1 AND t.relname = $2 AND NOT x.indisprimary`,
          [namespace, testCase.table],
        );
        assert.deepEqual(
          rows.map((row: { amname: string }) => row.amname),
          [testCase.what.startsWith("vector") ? "ivfflat" : "gist"],
          `${testCase.what}: PostgreSQL must keep its native access method`,
        );
      } finally {
        await client
          .query(
            `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
             DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
          )
          .catch(() => {});
        rmSync(work, { recursive: true, force: true });
      }
    }
  } finally {
    await client.end().catch(() => {});
  }
});
