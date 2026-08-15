// Apply must not call an already-applied MySQL migration "skipped" while its
// rollback marker says that `down` may have only partially landed. PostgreSQL
// has transactional rollback and no such marker; its ordinary re-apply remains
// a silent, successful skip.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { ADDON_PATH } from "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const OWNER_APP = "app_apply_unwind";

const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "create_parent";
export default {
  schema() {
    table("parent").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "apply-unwind-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ parent: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_create_parent.ts"), MIGRATION);
  return work;
}

function cli(
  work: string,
  schema: string,
  databaseUrl: string,
  verb: readonly string[] = ["apply", "--approve"],
) {
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...verb,
      "--dir", join(work, "migrations"),
      "--database-url", databaseUrl,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", schema,
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
  return new Promise<{ code: number | null; text: string }>((done) =>
    child.on("close", (code) =>
      done({ code, text: `${out}\n${err}`.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

function applyReport(text: string): { applied: string[]; skipped: string[] } {
  const line = text.split("\n").find((candidate) => candidate.startsWith("apply "));
  assert.ok(line, `apply must print its report; ${text}`);
  const separator = line.indexOf(": ");
  assert.notEqual(separator, -1, `apply report must contain JSON; ${line}`);
  return JSON.parse(line.slice(separator + 2)) as { applied: string[]; skipped: string[] };
}

test("apply refuses a MySQL version with an interrupted-unwind marker", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; apply rollback-marker coverage skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const connection = await mysql.createConnection({ uri: MYSQL_URL });
  const database = uniqueNamespace("apply_unwind_my");
  const meta = `${database}_migrations`;
  const base = MYSQL_URL.replace(/\/[^/]*$/, "");
  const work = project(database);

  try {
    await connection.query(`CREATE DATABASE \`${database}\``);
    const first = await cli(work, database, `${base}/${database}`);
    assert.equal(first.code, 0, `the migration must apply; ${first.text}`);
    const firstReport = applyReport(first.text);
    assert.equal(firstReport.applied.length, 1, `the first apply must apply one migration; ${first.text}`);

    // CONTROL: an ordinary re-apply is still the quiet, idempotent path.
    const clean = await cli(work, database, `${base}/${database}`);
    assert.equal(clean.code, 0, `a marker-free re-apply must succeed; ${clean.text}`);
    const cleanReport = applyReport(clean.text);
    assert.deepEqual(cleanReport.applied, []);
    assert.deepEqual(cleanReport.skipped, firstReport.applied);

    const [journalRows] = await connection.query(
      `SELECT version FROM \`${meta}\`.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq DESC LIMIT 1`,
    );
    const journal = journalRows as Array<{ version: string }>;
    assert.equal(journal.length, 1, "the apply must have written one current journal row");
    const version = journal[0].version;

    // Every marker field comes from that applied journal event, including its
    // timestamp and actor. Only the table mapping is supplied by the test.
    await connection.query(
      `INSERT INTO \`${meta}\`.schema_migrations_rollback_inflight
         (version, name, checksum, started_at, applied_by)
       SELECT version, name, checksum, \`at\`, \`by\`
         FROM \`${meta}\`.schema_migrations
        WHERE event_kind = 'applied' AND version = ?
        ORDER BY event_seq DESC LIMIT 1`,
      [version],
    );

    const refused = await cli(work, database, `${base}/${database}`);
    assert.equal(refused.code, 1, `the marked re-apply must fail closed; ${refused.text}`);
    assert.doesNotMatch(
      refused.text,
      /"applied":\[\],"skipped":\[[^\]]+\]/,
      `an unresolved unwind must not be reported as a clean skip; ${refused.text}`,
    );
    assert.match(refused.text, /rollback marker from an interrupted unwind/);
    assert.match(refused.text, new RegExp(version));
    assert.match(
      refused.text,
      new RegExp("DELETE FROM `" + meta + "`\\.schema_migrations_rollback_inflight"),
      `the refusal must print the existing operator repair; ${refused.text}`,
    );
    assert.match(refused.text, /run apply again/);
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${database}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${meta}\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("PostgreSQL re-apply is unaffected by MySQL rollback markers", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL control skipped");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace("apply_unwind_pg");
  const work = project(schema);

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const first = await cli(work, schema, PG_URL);
    assert.equal(first.code, 0, `the PostgreSQL migration must apply; ${first.text}`);
    const firstReport = applyReport(first.text);
    assert.equal(firstReport.applied.length, 1);

    const markerTable = await client.query<{ marker: string | null }>(
      "SELECT to_regclass($1) AS marker",
      [`${schema}_migrations.schema_migrations_rollback_inflight`],
    );
    assert.equal(markerTable.rows[0].marker, null, "PostgreSQL must not create the MySQL marker");

    const clean = await cli(work, schema, PG_URL);
    assert.equal(clean.code, 0, `PostgreSQL re-apply must keep succeeding; ${clean.text}`);
    const cleanReport = applyReport(clean.text);
    assert.deepEqual(cleanReport.applied, []);
    assert.deepEqual(cleanReport.skipped, firstReport.applied);
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

test("status reports the interrupted unwind that apply refuses over", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; status marker coverage skipped");
    return;
  }
  // F661. apply and rollback both refuse over this marker. status said
  // "1 applied, 0 pending", exit 0 - so the two verbs contradicted each other on
  // the same database, and the verb an operator reaches for FIRST was the one
  // telling them nothing was wrong.
  const mysql = (await import("mysql2/promise")).default;
  const connection = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = `f661_${Date.now().toString(36)}`;
  const meta = `${database}_migrations`;
  const work = project(database);
  const url = `${MYSQL_URL.slice(0, MYSQL_URL.lastIndexOf("/"))}/${database}`;
  try {
    await connection.query(`CREATE DATABASE \`${database}\``);
    const applied = await cli(work, database, url);
    assert.equal(applied.code, 0, `apply: ${applied.text}`);

    // CONTROL FIRST, in the same database, so the marker is the only variable.
    const clean = await cli(work, database, url, ["status", "--strict"]);
    assert.equal(clean.code, 0, `a marker-free strict status must pass; ${clean.text}`);
    assert.doesNotMatch(clean.text, /interrupted unwind/);

    await connection.query(
      `INSERT INTO \`${meta}\`.schema_migrations_rollback_inflight
         (version, name, checksum, started_at, applied_by)
       SELECT version, name, checksum, \`at\`, \`by\`
         FROM \`${meta}\`.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq DESC LIMIT 1`,
    );

    const marked = await cli(work, database, url, ["status"]);
    assert.equal(marked.code, 0, `plain status still reports rather than gates; ${marked.text}`);
    assert.match(
      marked.text,
      /interrupted unwind: mig_\w+ has a rollback marker/,
      `status must name the version and the state; ${marked.text}`,
    );

    // A strict gate that passed here would hand the pipeline to an apply that
    // refuses, which is the opposite of what strict is for.
    const strict = await cli(work, database, url, ["status", "--strict"]);
    assert.equal(strict.code, 1, `strict status must fail closed; ${strict.text}`);

    // F659's lesson: the JSON shape is what a CI gate reads.
    const json = await cli(work, database, url, ["status", "--json"]);
    const body = json.text.slice(json.text.indexOf("{"), json.text.lastIndexOf("}") + 1);
    const payload = JSON.parse(body) as { interruptedUnwinds?: string[] };
    assert.ok(
      (payload.interruptedUnwinds ?? []).length === 1,
      `--json must carry it too; got ${json.text}`,
    );
  } finally {
    await connection.query(`DROP DATABASE IF EXISTS \`${database}\``).catch(() => {});
    await connection.query(`DROP DATABASE IF EXISTS \`${meta}\``).catch(() => {});
    await connection.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
