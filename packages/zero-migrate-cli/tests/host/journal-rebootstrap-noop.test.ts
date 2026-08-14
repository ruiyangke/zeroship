// Re-bootstrapping an existing journal must write NOTHING to the catalog.
//
// `ensure_journal` runs on every invocation of every verb, and it is built to be
// idempotent. Idempotent was not enough: `CREATE OR REPLACE FUNCTION` rewrites
// its `pg_proc` row even when the body is already byte-identical, so two
// concurrent invocations collided on that row and the loser died with
//
//   zero-migrate: journal db error: tuple concurrently updated
//
// which reads like corruption and is only contention. It was observed through
// the CLI on `apply` and on `rollback`, against a project whose journal was
// ALREADY bootstrapped - so it was not a first-deploy problem that a warm-up run
// could avoid. It was every invocation, of every verb, for the life of a project.
//
// The other bootstrap steps never had this: `CREATE SCHEMA/TABLE IF NOT EXISTS`
// and the `pg_trigger`-guarded `CREATE TRIGGER` touch nothing once their object
// exists. Measured directly against PostgreSQL, only the unguarded
// `CREATE OR REPLACE FUNCTION` raced on the already-exists path. So the fix was
// to guard it the way its neighbour twenty lines below was already guarded.
//
// THE FIRST TEST IS THE REGRESSION GUARD, and it is deterministic rather than a
// race. `xmin` is the transaction that last wrote a row, so it is a direct
// witness of "was this row rewritten": if the guard is ever removed, re-bootstrap
// rewrites `pg_proc` and `xmin` moves on every run. Its CONTROL ARM issues the
// unguarded statement by hand and asserts `xmin` DOES move - without that, an
// assertion that `xmin` stayed put would also pass if `xmin` were something this
// test simply cannot observe.
//
// The second test is the race itself. It is the shape that was actually
// reported, but it can only ever be evidence, not proof - a race that fails to
// reproduce proves nothing. The first test is what will catch a regression.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "create_one";
export default {
  schema() {
    table("t1").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "rebootstrap-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }
`,
  );
  writeFileSync(join(work, "migrations", "20260101000000_create_one.ts"), MIGRATION);
  return work;
}

function runCli(
  work: string,
  schema: string,
  verb: string,
): Promise<{ code: number | null; err: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, verb,
        "--dir", join(work, "migrations"),
        "--database-url", pgUrl(),
        "--policy", join(work, "policy.toml"),
        "--schema", schema,
        "--owner-app", "app_rebootstrap",
      ],
      { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
    );
    let err = "";
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        err: err.split("\n").filter((line) => line && !line.startsWith("WARNING")).join(" "),
      }),
    );
  });
}

/** The immutability trigger function the bootstrap installs, named after the meta schema. */
const triggerFunctionOf = (meta: string): string => `${meta}_schema_migrations_immutable`;

test("re-bootstrapping an existing journal rewrites no catalog row", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("rebootstrap_noop");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  const fn = triggerFunctionOf(meta);
  const xmin = async (): Promise<string | undefined> => {
    const { rows } = await client.query(`SELECT xmin::text AS x FROM pg_proc WHERE proname = $1`, [
      fn,
    ]);
    assert.equal(rows.length, 1, `exactly one ${fn} must exist`);
    return rows[0].x as string;
  };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const first = await runCli(work, schema, "apply");
    assert.equal(first.code, 0, `the first apply must succeed; ${first.err}`);

    const installed = await xmin();

    // Two more invocations, each of which re-runs the whole bootstrap.
    for (const verb of ["status", "status"]) {
      const result = await runCli(work, schema, verb);
      assert.equal(result.code, 0, `${verb} must succeed; ${result.err}`);
    }
    assert.equal(
      await xmin(),
      installed,
      "re-bootstrap must not rewrite the trigger function's catalog row",
    );

    // CONTROL. Issue the unguarded statement by hand, with the SAME body the
    // engine installs, and confirm xmin moves anyway - that is precisely the
    // write the guard exists to avoid, and it is what the assertion above would
    // be blind to if xmin were not observable here.
    await client.query(
      `CREATE OR REPLACE FUNCTION "${fn}"() RETURNS trigger AS $fn$
         BEGIN
             RAISE EXCEPTION 'migration journal is append-only (no UPDATE/DELETE)';
         END;
         $fn$ LANGUAGE plpgsql`,
    );
    assert.notEqual(
      await xmin(),
      installed,
      "an UNGUARDED create-or-replace rewrites the row even when the body is unchanged - " +
        "if this does not move, the assertion above proves nothing",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("concurrent invocations against a bootstrapped journal report no catalog error", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("rebootstrap_race");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const first = await runCli(work, schema, "apply");
    assert.equal(first.code, 0, `the first apply must succeed; ${first.err}`);

    // Rounds of three, because the collision needs two bootstraps to overlap and
    // a single pair is easy to miss.
    for (let round = 0; round < 4; round += 1) {
      const results = await Promise.all([
        runCli(work, schema, "status"),
        runCli(work, schema, "status"),
        runCli(work, schema, "status"),
      ]);
      for (const result of results) {
        assert.doesNotMatch(
          result.err,
          /tuple concurrently updated|duplicate key value violates unique constraint "pg_/,
          "a concurrent invocation must not surface a raw PostgreSQL catalog error",
        );
        assert.equal(result.code, 0, `every concurrent status must succeed; ${result.err}`);
      }
    }
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
