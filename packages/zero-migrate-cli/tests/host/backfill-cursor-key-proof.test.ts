// The cursor key proof, measured case by case against a live server.
//
// `docs/writing-migrations.md` states the rule that keeps a windowed backfill from
// losing rows: "the planner proves that `cursorColumns` is the exact ordered,
// non-null column tuple of a primary or full unique candidate key ... Prefix,
// partial, expression, nullable, reordered, and wider keys do not prove the
// cursor."
//
// That sentence names SIX rejections, and every one of them is a data-safety
// claim rather than a usability one. A windowed backfill checkpoints on the cursor
// and resumes with `WHERE cursor > last`. If the cursor is not unique, two rows
// sharing a value can straddle a batch boundary and the second is never visited:
// the migration reports success, the journal records a completed backfill, and the
// rows are silently unwritten. A prefix of a composite key is exactly that
// situation, and so is a partial or expression index, which does not constrain the
// bare column at all.
//
// The existing backfill files cover the ordering defect
// (`backfill-cursor-ordering`) and the per-dialect column KIND rules
// (`backfill-cursor-column-kind`). Neither exercises the key SHAPE, so nothing
// pinned the six.
//
// EVERY ARM IS MEASURED, none inferred from the others: a proof that rejects a
// prefix need not reject a reordering, and a proof implemented as "is there a
// unique index mentioning these columns" would accept four of the six.
//
// THE CONTROL IS THE POINT. `accepts an exact primary key` runs the identical
// migration against the identical shape with a cursor that DOES prove, and asserts
// the backfill actually wrote every row. Without it, a planner that refused every
// backfill outright would pass all six refusal arms.
//
// Refusals are asserted by MESSAGE, not by "it threw". These fixtures create their
// tables out of band, so a typo in the DDL, an unregistered table, or a policy gap
// would all throw too, and each would look like a passing refusal.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_cursor_key_proof";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function charter(scopeName: string): string {
  const scope = `{ include = [${JSON.stringify(scopeName)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

/** One backfill, cursored on whatever tuple the case is testing. `batchSize: 2`
 *  over three rows means a real window boundary rather than a single batch that
 *  would hide a cursor problem entirely. */
function bump(cursorColumns: string[]): MigrationModule {
  return {
    default: {
      up() {
        table("rows_t").backfill({
          set: { val: (col) => col("val").add(1) },
          where: (col) => col("val").ge(0),
          cursorColumns,
          cursorStability: { mode: "externalInvariant", name: "rows_cursor_immutable" },
          batchSize: 2,
          name: "bump_all",
        });
      },
    },
  } as MigrationModule;
}

interface Case {
  readonly label: string;
  readonly ddl: string;
  /** Extra statements (indexes) run after the table exists. `@T` is the table. */
  readonly extra?: readonly string[];
  readonly seed: string;
  readonly cursor: string[];
  /** The refusal this shape must produce. Matching only /cursor/ would let an
   *  unrelated cursor-mentioning failure stand in for the key proof, and the
   *  nullable case genuinely refuses for a different, narrower reason. */
  readonly expect: RegExp;
}

const NOT_A_KEY = /the exact ordered tuple is not a complete PRIMARY KEY or non-par/;

/** Each shape is the smallest one that isolates a single reason the proof must
 *  fail. Every table carries `val`, so the backfill itself is identical throughout
 *  and only the KEY differs. */
const REFUSED: readonly Case[] = [
  {
    label: "a prefix of a composite primary key",
    ddl: `a int NOT NULL, b int NOT NULL, val int NOT NULL, PRIMARY KEY (a, b)`,
    seed: `INSERT INTO @T (a, b, val) VALUES (1,1,0),(1,2,0),(2,1,0)`,
    // `a` alone repeats, which is precisely how rows go missing.
    cursor: ["a"],
    expect: NOT_A_KEY,
  },
  {
    label: "a tuple wider than the key it claims",
    ddl: `a int NOT NULL, b int NOT NULL, val int NOT NULL, PRIMARY KEY (a)`,
    seed: `INSERT INTO @T (a, b, val) VALUES (1,1,0),(2,2,0),(3,3,0)`,
    cursor: ["a", "b"],
    expect: NOT_A_KEY,
  },
  {
    label: "a reordering of a composite primary key",
    ddl: `a int NOT NULL, b int NOT NULL, val int NOT NULL, PRIMARY KEY (a, b)`,
    seed: `INSERT INTO @T (a, b, val) VALUES (1,1,0),(1,2,0),(2,1,0)`,
    cursor: ["b", "a"],
    expect: NOT_A_KEY,
  },
  {
    label: "a nullable unique column",
    ddl: `id int NOT NULL PRIMARY KEY, c int NULL UNIQUE, val int NOT NULL`,
    seed: `INSERT INTO @T (id, c, val) VALUES (1,10,0),(2,20,0),(3,30,0)`,
    cursor: ["c"],
    // Narrower and distinct: nullability is checked per component.
    expect: /cursor component "c" is nullable; every component must be NOT NULL/,
  },
  {
    label: "a partial unique index",
    ddl: `id int NOT NULL PRIMARY KEY, c int NOT NULL, val int NOT NULL`,
    extra: [`CREATE UNIQUE INDEX rows_c_partial ON @T (c) WHERE c > 0`],
    seed: `INSERT INTO @T (id, c, val) VALUES (1,10,0),(2,20,0),(3,30,0)`,
    cursor: ["c"],
    expect: NOT_A_KEY,
  },
  {
    label: "an expression unique index",
    ddl: `id int NOT NULL PRIMARY KEY, c text NOT NULL, val int NOT NULL`,
    extra: [`CREATE UNIQUE INDEX rows_c_expr ON @T (lower(c))`],
    seed: `INSERT INTO @T (id, c, val) VALUES (1,'a',0),(2,'b',0),(3,'c',0)`,
    cursor: ["c"],
    expect: NOT_A_KEY,
  },
];

async function runCase(
  client: Awaited<ReturnType<typeof connectLivePg>>,
  testCase: Case,
): Promise<{ error: string | null; values: number[] }> {
  const schema = uniqueNamespace("cursor_proof_pg");
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  try {
    await client!.query(`CREATE SCHEMA "${schema}"`);
    const qualified = `"${schema}".rows_t`;
    await client!.query(`CREATE TABLE ${qualified} (${testCase.ddl})`);
    for (const statement of testCase.extra ?? []) {
      await client!.query(statement.replaceAll("@T", qualified));
    }
    await client!.query(testCase.seed.replaceAll("@T", qualified));

    const error = await apply({
      migration: bump(testCase.cursor),
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: { rows_t: OWNER_APP },
      policy: [charter(schema)],
      approved: true,
      appliedBy: "backfill-cursor-key-proof",
      nameFallback: "bump",
    }).then(
      () => null,
      (thrown: unknown) => String((thrown as Error)?.message ?? thrown),
    );

    const { rows } = await client!.query(`SELECT val FROM ${qualified} ORDER BY val`);
    return { error, values: rows.map((row) => Number(row.val)) };
  } finally {
    await client!
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
  }
}

for (const testCase of REFUSED) {
  test(`PostgreSQL refuses a backfill cursored on ${testCase.label}`, async (ctx) => {
    const client = await connectLivePg(ctx);
    if (!client) return;
    try {
      const { error, values } = await runCase(client, testCase);

      assert.ok(error, `the backfill must be refused; it succeeded instead`);
      // By MESSAGE: these fixtures build their tables by hand, so a bad fixture
      // would throw too and would otherwise read as a passing refusal.
      assert.match(
        error,
        /planner refused resumable backfill/,
        `the refusal must come from the planner's cursor proof: ${error}`,
      );
      assert.match(
        error,
        testCase.expect,
        `and must give the reason this shape actually fails for: ${error}`,
      );
      // It must name the offending tuple, so an operator can act on it.
      assert.match(
        error,
        new RegExp(testCase.cursor.map((column) => `"${column}"`).join(", ")),
        `the refusal must name the cursor tuple it rejected: ${error}`,
      );
      // The refusal must land BEFORE any row is touched. A backfill that refuses
      // only after writing part of the cohort is the data-loss shape this rule
      // exists to prevent.
      assert.deepEqual(
        values,
        [0, 0, 0],
        `a refused backfill must not have written any row; got ${JSON.stringify(values)}`,
      );
    } finally {
      await client.end().catch(() => {});
    }
  });
}

/** The control. Without it every arm above would pass against a planner that
 *  refused all backfills, and the six refusals would say nothing. */
test("PostgreSQL accepts a backfill cursored on an exact primary key", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  try {
    const { error, values } = await runCase(client, {
      label: "exact primary key",
      expect: /never used/,
      ddl: `a int NOT NULL, b int NOT NULL, val int NOT NULL, PRIMARY KEY (a, b)`,
      seed: `INSERT INTO @T (a, b, val) VALUES (1,1,0),(1,2,0),(2,1,0)`,
      cursor: ["a", "b"],
    });

    assert.equal(error, null, `the exact key must prove the cursor; got ${error}`);
    // Every row visited EXACTLY once. The transform is `val + 1` from a seeded 0,
    // so the stored value is the visit count and neither a skip nor a double-apply
    // can hide behind an idempotent-looking result.
    assert.deepEqual(
      values,
      [1, 1, 1],
      `every row must be written exactly once; got ${JSON.stringify(values)}`,
    );
  } finally {
    await client.end().catch(() => {});
  }
});
