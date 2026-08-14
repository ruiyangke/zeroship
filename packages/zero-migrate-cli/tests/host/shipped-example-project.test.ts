// The shipped `hr-system` example, applied end to end against live PostgreSQL.
//
// `packages/zero-migrate-cli/hr-system/` is a complete 17-migration project with
// its own policy and registry - the thing a reader copies to start. It is
// referenced by NOTHING: no test, no package script, no doc. If a change broke it,
// the suite would stay green and the breakage would ship.
//
// `hr_sqlite.rs` looks like coverage and is not. It applies a CAPTURED IR SNAPSHOT
// (`tests/fixtures/hr/migrations.json`) rather than this directory, so it cannot
// notice the project drifting from the snapshot. And it runs on SQLite, where the
// set's `renameColumn` lowers to a table rebuild - so the PostgreSQL path this
// example is actually written for, the two-deploy expand/contract, is exercised by
// neither.
//
// That last point is what makes this worth a live PG run rather than a lint: the
// final migration opens a pending contract, and the realistic thing that follows
// is resolving it. The set seeds real rows first (5 employees), backfills a
// derived column, updates, deletes, and only then renames - so the rename runs
// over data that earlier migrations in the same project produced.
//
// THE VALUES ARE THE ASSERTION. Confirming the column was renamed would pass on a
// commit that dropped the old column without carrying anything across; the salary
// figures have to arrive intact on the other side.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only - the online rename is
// PostgreSQL's.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const EXAMPLE = resolve(HERE, "../../hr-system");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_hr";
const RENAME_MIGRATION = "rename_salary_column";

/** Every table the example is expected to build. */
const EXPECTED_TABLES = [
  "departments",
  "employee_position_history",
  "employees",
  "job_grades",
  "leave_requests",
  "payroll_items",
  "payroll_runs",
  "positions",
];

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function runCli(
  schema: string,
  argv: string[],
): Promise<{ code: number | null; out: string; err: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, ...argv,
        "--dir", join(EXAMPLE, "migrations"),
        "--database-url", pgUrl(),
        "--policy", join(EXAMPLE, "no-inject.toml"),
        "--registry", join(EXAMPLE, "registry.json"),
        "--schema", schema,
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: EXAMPLE,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let err = "";
    let out = "";
    child.stderr.on("data", (chunk) => (err += chunk));
    child.stdout.on("data", (chunk) => (out += chunk));
    child.on("close", (code) =>
      resolvePromise({ code, out, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    );
  });
}

test("the shipped hr-system example applies and its rename resolves, carrying the data", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("hrexample");
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    const applied = await runCli(schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `the shipped example must apply; ${applied.err}`);

    // Every table the project declares, and no fewer.
    const { rows: tables } = await client.query(
      `SELECT table_name FROM information_schema.tables
        WHERE table_schema = $1 ORDER BY table_name`,
      [schema],
    );
    assert.deepEqual(
      tables.map((row) => row.table_name),
      EXPECTED_TABLES,
      "the example must build exactly the tables it declares",
    );

    // The seeds ran, so the rename below operates on real rows rather than an
    // empty table - which is the difference between this and a lint.
    const { rows: seeded } = await client.query(
      `SELECT count(*)::int AS n FROM "${schema}".employees`,
    );
    assert.ok(seeded[0].n > 0, "the example's seed migrations must have inserted employees");

    // The final migration opens the online rename, and the CLI says so.
    assert.match(
      applied.out,
      /"pendingContracts":\[\{[^]*"fromColumn":"base_salary"[^]*"toColumn":"annual_base_salary"/,
      `the example's last migration must open the rename contract; got: ${applied.out.slice(-400)}`,
    );

    const salariesBefore = await client.query(
      `SELECT id, base_salary FROM "${schema}".employees ORDER BY id`,
    );
    assert.ok(
      salariesBefore.rows.some((row) => row.base_salary !== null),
      "the window must open over rows that actually carry a salary",
    );

    // Resolve it, the way the project's own README-less workflow would.
    const committed = await runCli(schema, [
      "resolve", RENAME_MIGRATION, "--commit", "--approve",
    ]);
    assert.equal(committed.code, 0, `the rename must commit; ${committed.err}`);

    const { rows: columns } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'employees'
          AND column_name IN ('base_salary', 'annual_base_salary')
        ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      columns.map((row) => row.column_name),
      ["annual_base_salary"],
      "the commit must drop the source column and keep the destination",
    );

    // THE VALUES. A commit that dropped the old column without carrying anything
    // across would satisfy every assertion above.
    const salariesAfter = await client.query(
      `SELECT id, annual_base_salary FROM "${schema}".employees ORDER BY id`,
    );
    assert.deepEqual(
      salariesAfter.rows.map((row) => [Number(row.id), row.annual_base_salary]),
      salariesBefore.rows.map((row) => [Number(row.id), row.base_salary]),
      "every salary must survive the rename it was carried through",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});
