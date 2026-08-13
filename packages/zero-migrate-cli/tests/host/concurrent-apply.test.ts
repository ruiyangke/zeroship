// Two apply processes racing the same project.
//
// This is the ordinary CI shape - two jobs from the same merge, or a retry that
// overlaps its predecessor - and it separates cleanly into two questions.
//
// THE SAFETY PROPERTY HOLDS. No migration is applied twice, no version is
// journaled twice, and the schema ends up exactly as one run would have left it.
// The project lock does its job, and the first test asserts that.
//
// THE ROBUSTNESS PROPERTY DOES NOT. The journal bootstrap runs BEFORE the project
// lock serialises anything, so two fresh processes race to create the journal
// namespace and its types. The loser surfaces PostgreSQL's own catalog error:
//
//   duplicate key value violates unique constraint "pg_namespace_nspname_index"
//   duplicate key value violates unique constraint "pg_type_typname_nsp_index"
//   tuple concurrently updated
//
// None of those tells an operator what happened. They are benign contention -
// re-running succeeds - but they read like corruption, and nothing in the CLI
// output or the docs says otherwise. PostgreSQL's own `CREATE ... IF NOT EXISTS`
// is racy in exactly this way, so tolerating these codes is the standard fix
// rather than a novel one.
//
// The second test records that as TODAY's behaviour and is written to fail when
// it improves. Deciding which catalog error codes are benign enough to swallow is
// the judgement in that fix - swallow too broadly and a real failure is hidden -
// so it is recorded rather than improvised.
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

/** Twelve tables, so the window a racer can land in is wide. */
const MIGRATION = `import { table, t } from "zero-migrate";
export const name = "create_many";
export default {
  up() {
    for (const n of [1,2,3,4,5,6,7,8,9,10,11,12]) {
      table("t" + n).create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    }
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "concurrent-"));
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
  writeFileSync(join(work, "migrations", "20260101000000_create_many.ts"), MIGRATION);
  return work;
}

function spawnApply(work: string, schema: string): Promise<{ code: number | null; err: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply",
        "--dir", join(work, "migrations"),
        "--database-url", pgUrl(),
        "--policy", join(work, "policy.toml"),
        "--schema", schema,
        "--owner-app", "app_concurrent",
      ],
      {
        cwd: work,
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    let err = "";
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({
        code,
        err: err
          .split("\n")
          .filter((line) => line && !line.startsWith("WARNING"))
          .join(" "),
      }),
    );
  });
}

test("racing applies never double-apply: one wins, the journal holds each version once", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("concurrent_safe");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    // Bootstrap the journal first, so this test measures the LOCK rather than the
    // bootstrap race the next test is about.
    const first = await spawnApply(work, schema);
    assert.equal(first.code, 0, `the first apply must succeed; ${first.err}`);

    const [a, b] = await Promise.all([spawnApply(work, schema), spawnApply(work, schema)]);

    // Whatever the exit codes, the DATA must be untouched by the race.
    const { rows: duplicated } = await client.query(
      `SELECT count(*)::int AS n FROM (
         SELECT version FROM "${meta}".schema_migrations
          WHERE event_kind = 'applied'
          GROUP BY version HAVING count(*) > 1) AS d`,
    );
    assert.equal(
      duplicated[0].n,
      0,
      `no version may be journaled twice; exits ${a.code}/${b.code}, ${a.err} ${b.err}`,
    );

    const { rows: tables } = await client.query(
      `SELECT count(*)::int AS n FROM information_schema.tables WHERE table_schema = $1`,
      [schema],
    );
    assert.equal(tables[0].n, 12, "the schema must look exactly as a single run would leave it");
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

test("TODAY a racing first deploy can fail with a raw PostgreSQL catalog error", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("concurrent_boot");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    // No prior apply: both processes race the JOURNAL BOOTSTRAP, which happens
    // before the project lock exists to serialise them.
    const [a, b] = await Promise.all([spawnApply(work, schema), spawnApply(work, schema)]);
    const failures = [a, b].filter((r) => r.code !== 0);

    // The safety property still holds even here - that part is not the complaint.
    const { rows: duplicated } = await client.query(
      `SELECT count(*)::int AS n FROM (
         SELECT version FROM "${meta}".schema_migrations
          WHERE event_kind = 'applied'
          GROUP BY version HAVING count(*) > 1) AS d`,
    );
    assert.equal(duplicated[0].n, 0, "even in the bootstrap race, nothing may be applied twice");

    if (failures.length === 0) {
      // The race is real but not certain; when both win there is nothing to pin.
      return;
    }

    // What TODAY looks like: PostgreSQL's own catalog error, verbatim. When the
    // bootstrap learns to tolerate these, this assertion fails - and the fix
    // should replace it with one asserting a contention message an operator can
    // act on, or a clean success.
    assert.match(
      failures[0].err,
      /duplicate key value violates unique constraint "pg_(namespace|type)_|tuple concurrently updated/,
      `the loser surfaces a raw catalog error today; got: ${failures[0].err}`,
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
