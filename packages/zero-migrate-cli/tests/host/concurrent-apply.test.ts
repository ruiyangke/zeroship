// Two apply processes racing the same project.
//
// This is the ordinary CI shape - two jobs from the same merge, or a retry that
// overlaps its predecessor - and it separates cleanly into two questions.
//
// THE SAFETY PROPERTY HOLDS. No migration is applied twice, no version is
// journaled twice, and the schema ends up exactly as one run would have left it.
// The project lock does its job, and the first test asserts that.
//
// THE ROBUSTNESS PROPERTY NOW HOLDS TOO, and the second test pins it. It did not
// when this file was written: the journal bootstrap ran BEFORE the project lock
// serialised anything, so two fresh processes raced to CREATE the journal
// namespace and its types, and the loser surfaced PostgreSQL's own catalog
// error:
//
//   duplicate key value violates unique constraint "pg_namespace_nspname_index"
//   duplicate key value violates unique constraint "pg_type_typname_nsp_index"
//   tuple concurrently updated
//   trigger "zs_immutable_truncate_trg" for relation "..." already exists
//
// None of those tells an operator what happened. They are benign contention -
// re-running succeeds - but they read like corruption, and nothing in the CLI
// output or the docs said otherwise.
//
// The fix was not to tolerate those codes, and it needed no new lock. `verbs.rs`
// bootstrapped the journal BEFORE acquiring the project lock, which left the one
// window nothing serialized; the deploy verbs now take the lock first and
// bootstrap inside it. Serializing needs no judgement about which catalog errors
// are benign, and that judgement is what had kept the fix parked.
//
// The bootstrap moved INSIDE the lock bracket rather than merely after the
// acquisition, because the release runs after that block - bootstrapping outside
// it would leak the project lock on any bootstrap failure.
//
// MySQL never had this bug: its `ensure_journal` already took a dedicated
// bootstrap lock. The same race measured 0/3 there against 3/3 on PostgreSQL,
// which is what identified the missing serialization.
//
// FIRST DEPLOY IS NOW THE WHOLE OF IT. This file originally recorded the same
// errors against a project whose journal ALREADY existed, which made them
// unavoidable rather than a warm-up problem. That half had a separate cause - an
// unguarded `CREATE OR REPLACE FUNCTION` rewriting its catalog row on every
// invocation - and is fixed and pinned in `journal-rebootstrap-noop.test.ts`.
// What remains here is only the window before the objects exist at all, which is
// why the second test below deliberately starts from a fresh project.
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

    // FIXED by ordering: the deploy verbs acquire the project lock before
    // bootstrapping the journal, so the bootstrap is serialized by the lock that
    // already existed. Both processes complete - one bootstraps, the other waits
    // for the lock and then finds the journal present.
    //
    // Serializing beat tolerating SQLSTATEs, which is what this test used to
    // record as the open judgement. The set of colliding statements had already
    // grown once (the `pg_trigger`-guarded CREATE TRIGGER appeared after the
    // first three), and a swallow-list broad enough to cover an open-ended set
    // would hide real failures.
    assert.deepEqual(
      failures.map((failure) => failure.err),
      [],
      "a racing first deploy must not surface a raw catalog error",
    );

    // Asserted on the SHAPE the failure used to take, so a regression that
    // reintroduces it is named rather than just counted.
    for (const result of [a, b]) {
      assert.doesNotMatch(
        result.err,
        /duplicate key value violates unique constraint "pg_|tuple concurrently updated|already exists/,
        `no raw PostgreSQL catalog error may reach the operator; got: ${result.err}`,
      );
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
