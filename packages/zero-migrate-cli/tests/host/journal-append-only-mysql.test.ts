// What the append-only journal guard actually stops on MySQL — including the one
// thing it cannot.
//
// `journal-append-only.test.ts` pins the guarantee on PostgreSQL: `UPDATE`,
// `DELETE` and `TRUNCATE` are all refused, even for the role that owns the table.
// Its header argues that this makes append-only an ENFORCED property rather than a
// convention, so a credential over-granted by mistake still cannot rewrite history.
//
// THAT ARGUMENT DOES NOT SURVIVE THE CROSSING TO MySQL, and nothing measured it.
// MySQL triggers do not fire on `TRUNCATE TABLE` — the server treats it as DDL
// rather than a row operation — so no trigger can intercept it. Measured here:
//
//   UPDATE     refused, `migration journal is append-only (no UPDATE/DELETE)`
//   DELETE     refused, same message
//   TRUNCATE   ALLOWED — the journal goes from one row to zero
//
// This test asserts that reality rather than the guarantee, because asserting the
// guarantee would just fail. It exists for three reasons:
//
//   1. the two refusals are real and worth protecting — losing them would be a
//      silent downgrade on the target where the third is already missing;
//   2. the TRUNCATE arm DOCUMENTS the gap where someone will look for it, next to
//      the behaviour, instead of only in prose;
//   3. if MySQL or the engine ever gains a way to refuse it, this test FAILS and
//      the gap can be closed deliberately — invert the arm, do not delete it.
//
// The operational consequence is in `docs/security-model.md`: on MySQL,
// withholding the `TRUNCATE` grant is not defence in depth, it is the only
// control. On PostgreSQL it is a second layer behind the trigger.
//
// GATE: `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_journal";

const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "creates_one_table";
export default {
  schema() {
    table("t1").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

function uniqueDatabase(): string {
  return `jrnl_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(database: string): string {
  const work = mkdtempSync(join(HERE, "jrnlmy-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(database)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(database)}] }
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ t1: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_m.ts"), MIGRATION);
  return work;
}

function apply(work: string, database: string) {
  const base = String(MYSQL_URL).replace(/\/[^/]*$/, "");
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", `${base}/${database}`,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", database,
      "--owner-app", OWNER_APP,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  let err = "";
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; err: string }>((done) =>
    child.on("close", (code) =>
      done({ code, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("MySQL refuses UPDATE and DELETE on the journal, but cannot refuse TRUNCATE", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL journal-guard coverage skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const connection = await mysql.createConnection({ uri: MYSQL_URL });

  const database = uniqueDatabase();
  const meta = `${database}_migrations`;
  const work = project(database);

  const rowCount = async (): Promise<number> => {
    const [rows] = await connection.query(
      `SELECT count(*) AS n FROM \`${meta}\`.schema_migrations`,
    );
    return Number((rows as Array<{ n: number }>)[0].n);
  };
  /** Runs `sql`, returning the error message or null when it succeeded. */
  const attempt = async (sql: string): Promise<string | null> => {
    try {
      await connection.query(sql);
      return null;
    } catch (error) {
      return (error as Error).message;
    }
  };

  try {
    await connection.query(`CREATE DATABASE \`${database}\``);
    const ran = await apply(work, database);
    assert.equal(ran.code, 0, `the migration must apply; ${ran.err}`);
    assert.equal(await rowCount(), 1, "the journal must hold the applied event");

    // The two the trigger DOES cover. Asserted by message, not just by throwing:
    // a permissions error or a missing table would also throw.
    for (const [label, sql] of [
      ["UPDATE", `UPDATE \`${meta}\`.schema_migrations SET name = 'tampered'`],
      ["DELETE", `DELETE FROM \`${meta}\`.schema_migrations`],
    ] as const) {
      const failure = await attempt(sql);
      assert.ok(failure, `${label} must be refused on the journal`);
      assert.match(
        failure,
        /append-only/,
        `${label} must be refused BY THE GUARD, not incidentally: ${failure}`,
      );
    }
    assert.equal(await rowCount(), 1, "neither refusal may have changed the journal");

    // The one it cannot. This asserts the GAP; see the header before changing it.
    const truncated = await attempt(`TRUNCATE TABLE \`${meta}\`.schema_migrations`);
    assert.equal(
      truncated,
      null,
      "MySQL triggers do not fire on TRUNCATE, so this is expected to SUCCEED " +
        "today. If it now fails, the gap has been closed - invert this arm and " +
        "update docs/security-model.md, do not delete it",
    );
    assert.equal(
      await rowCount(),
      0,
      "and the consequence is total: one statement erases the history",
    );
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${database}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${meta}\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
