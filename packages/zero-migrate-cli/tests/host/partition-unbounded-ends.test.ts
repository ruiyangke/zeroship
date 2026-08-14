// `minValue` / `maxValue` really are unbounded, and rows route accordingly.
//
// These two sentinels are how a range-partitioned table gets catch-all ends:
// `from: [minValue]` on the lowest partition and `to: [maxValue]` on the highest,
// so no row falls outside every partition. They had no host coverage at all —
// `partition-collapse-scope.test.ts` covers the `collapse` opt-out and uses a
// `default: true` partition instead, which is a different mechanism.
//
// THE ASSERTION IS ROUTING, not the bound text. Reading
// `pg_get_expr(relpartbound)` back would confirm some bound was stored; it would
// not confirm the table behaves as the author intended. Rows are inserted and
// `tableoid` says which partition each one actually landed in, which is the thing
// an author is relying on.
//
// The values are chosen so a plausible wrong rendering fails:
//
//   i32::MIN   proves MINVALUE is genuinely unbounded. Had it rendered as a
//              literal — 0, or the column default — this row would match no
//              partition and PostgreSQL would REFUSE the insert outright.
//   -5, 0      ordinary values below the split
//   99 / 100   the half-open boundary. FROM is inclusive and TO is exclusive, so
//              99 belongs low and 100 belongs high; an off-by-one or an
//              inclusive-TO mistake puts 100 in the wrong partition while every
//              other row still looks right.
//   i32::MAX   the same proof for MAXVALUE at the top end.
//
// Partitioning is PostgreSQL-only in this engine, so there is no cross-dialect arm;
// `partition-collapse-scope.test.ts` covers what the other targets do with a
// partitioned declaration.
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

const OWNER_APP = "app_partends";
const TABLE = "mm_t";
const LOW = "mm_lo";
const HIGH = "mm_hi";

/** `[bucket, expected partition]`. The split is at 100. */
const ROUTING: ReadonlyArray<readonly [number, string]> = [
  [-2147483648, LOW], // i32::MIN — only reachable if MINVALUE is truly unbounded
  [-5, LOW],
  [0, LOW],
  [99, LOW], // half-open: FROM inclusive, TO exclusive
  [100, HIGH],
  [5000, HIGH],
  [2147483647, HIGH], // i32::MAX — the same proof for MAXVALUE
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "partends-"));
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
    JSON.stringify({ [TABLE]: OWNER_APP, [LOW]: OWNER_APP, [HIGH]: OWNER_APP }),
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t, minValue, maxValue } from "zero-migrate";
export const name = "a";
export default {
  schema() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), bucket: t.int().notNull() },
      primaryKey: ["id", "bucket"],
      partitionBy: { range: ["bucket"] },
    });
    table("${TABLE}").partition("${LOW}").create({ from: [minValue], to: [100] });
    table("${TABLE}").partition("${HIGH}").create({ from: [100], to: [maxValue] });
  },
};
`,
  );
  return work;
}

test("minValue and maxValue give the partition set unbounded ends", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("partends");
  const work = project();
  try {
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

    // An unroutable row is an ERROR here, not a silent misplacement, so a literal
    // rendering of MINVALUE would surface as a failed insert rather than a wrong
    // assertion below.
    for (const [index, [bucket]] of ROUTING.entries()) {
      await client.query(
        `INSERT INTO "${namespace}"."${TABLE}" (id, bucket) VALUES ($1, $2)`,
        [index + 1, bucket],
      );
    }

    const { rows } = await client.query(
      `SELECT bucket, tableoid::regclass::text AS part
         FROM "${namespace}"."${TABLE}" ORDER BY bucket`,
    );
    const actual = rows.map((row: { bucket: number; part: string }) => [
      Number(row.bucket),
      // `regclass` renders schema-qualified here; only the partition name matters.
      String(row.part).split(".").pop(),
    ]);
    const expected = [...ROUTING]
      .sort((a, b) => a[0] - b[0])
      .map(([bucket, part]) => [bucket, part]);

    assert.deepEqual(
      actual,
      expected,
      "every row must land in the partition its bound claims -- the boundary pair " +
        "99/100 is where an inclusive-TO mistake shows, and the i32 extremes are " +
        "where a literal rendering of MINVALUE/MAXVALUE would",
    );
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
