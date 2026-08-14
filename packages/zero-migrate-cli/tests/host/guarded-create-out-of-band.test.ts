// A guard against an object created OUT OF BAND, which is where the two dialects part.
//
// `docs/writing-migrations.md` used to say MySQL is "not probed" and that a guarded
// operation's statement "runs unconditionally, so a repeat run fails with the server's
// own duplicate-object or missing-object error". Both halves are wrong, and this pins
// what actually happens.
//
// MySQL DOES probe: `apply/backend/mysql/session.rs` calls
// `existence_probe::decide` and honours all three verdicts (RunBare / SatisfiedNoop /
// FailDrift), the same call PostgreSQL and SQLite make. What MySQL does not do is
// resolve the guard while the PENDING SCHEMA is projected, and the projection runs
// first — so a state the folded history disagrees with is refused by the guard-blind
// fold before the probe is ever reached.
//
// The shape has to be one the fold cannot resolve on its own, so the table is created
// OUT OF BAND: no migration in the history creates it, and the live catalog has it.
// A guarded create inside the history would be folded away and prove nothing.
//
// TWO ARMS, and the PostgreSQL one is the control. MySQL refusing alone would read as
// "a guarded create of an existing table is refused", which is dialect-independent and
// false — PostgreSQL absorbs the identical migration.
//
// The MySQL assertion is on the MESSAGE, not the exit code. The old documented
// behaviour ("runs unconditionally") ALSO fails, with MySQL's own duplicate-table
// error, so an exit-code-only test would pass against the behaviour this file exists
// to say we do not have.
//
// GATES: `ZERO_MIGRATE_TEST_PG_URL`, `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

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
const OWNER_APP = "app_oob";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** A prior migration is required: with an empty history no projection is built at all,
 *  and the guarded create would reach the probe on every dialect, hiding the split. */
function project(namespace: string): string {
  const work = mkdtempSync(join(HERE, "oobguard-"));
  mkdirSync(join(work, "m"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(namespace)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(namespace)}] }
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ anchor: OWNER_APP, oob: OWNER_APP }));
  writeFileSync(
    join(work, "m", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "anchor";
export default { schema() { table("anchor").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] }); } };
`,
  );
  writeFileSync(
    join(work, "m", "20260102000000_b.ts"),
    `import { table, t } from "zero-migrate";
export const name = "guarded_oob";
export default { schema() { table("oob").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"], ifNotExists: true }); } };
`,
  );
  return work;
}

function apply(work: string, url: string, namespace: string) {
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "m"), "--database-url", url,
      "--policy", join(work, "policy.toml"), "--registry", join(work, "registry.json"),
      "--schema", namespace, "--owner-app", OWNER_APP,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  let out = "";
  let err = "";
  child.stdout.on("data", (chunk) => (out += chunk));
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; text: string }>((done) =>
    child.on("close", (code) =>
      done({ code, text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("MySQL refuses a guarded create of an out-of-band table during projection", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL out-of-band guard arm skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const connection = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("oobguard_my");
  const work = project(database);

  try {
    await connection.query(`CREATE DATABASE \`${database}\``);
    await connection.query(`CREATE TABLE \`${database}\`.oob (id int PRIMARY KEY)`);

    const outcome = await apply(work, `${MYSQL_URL.replace(/\/[^/]*$/, "")}/${database}`, database);
    assert.equal(outcome.code, 1, `the guarded create must be refused; ${outcome.text}`);
    assert.match(
      outcome.text,
      /failed to project pending schema/,
      `the refusal must come from projection, which is what makes the guard unreachable ` +
        `on MySQL: ${outcome.text}`,
    );
    assert.match(
      outcome.text,
      /table `oob` already exists/,
      `and it must name the object: ${outcome.text}`,
    );
    // The documented-but-false behaviour would surface MySQL's own error instead.
    assert.doesNotMatch(
      outcome.text,
      /ER_TABLE_EXISTS_ERROR|Table 'oob' already exists/,
      `the statement must never have run, so MySQL's own duplicate-table error must ` +
        `not appear: ${outcome.text}`,
    );
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${database}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${database}_migrations\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("PostgreSQL absorbs the same out-of-band guarded create, the control for the MySQL arm", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PG out-of-band guard arm skipped");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace("oobguard_pg");
  const work = project(schema);

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    await client.query(`CREATE TABLE "${schema}".oob (id int PRIMARY KEY)`);

    const outcome = await apply(work, PG_URL, schema);
    assert.equal(
      outcome.code,
      0,
      `PostgreSQL resolves the guard while projecting, so the identical migration ` +
        `applies: ${outcome.text}`,
    );
    // The out-of-band table must survive untouched — "absorbed" means the create was a
    // satisfied no-op, not that anything was replaced.
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'oob'`,
      [schema],
    );
    assert.deepEqual(
      rows.map((row) => row.column_name),
      ["id"],
      "the satisfied no-op must leave the out-of-band table exactly as it was",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
