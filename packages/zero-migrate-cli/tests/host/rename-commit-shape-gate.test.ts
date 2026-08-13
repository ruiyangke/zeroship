// The two commit-time checks that stand between a drifted rename window and a
// dropped column.
//
// Committing a PostgreSQL online rename DROPS the old column. Before it does,
// `pending_contract_shape` establishes three separate facts, and `engine.rs`
// turns each into its own refusal:
//
//   columns_compatible    both columns exist with the same declared type
//   values_synchronized   no row where the two columns disagree
//   trigger_ready         the managed dual-write trigger exists and is ENABLED
//
// Only the first was ever measured against a database (`pg_scenarios.rs` drops
// the destination column out of band). The other two are the ones guarding the
// data: they are what stops a commit from destroying the only correct copy of a
// value after the coexistence window silently stopped working.
//
// `rename-dual-write-window.test.ts` drops the trigger to prove the mirror stops,
// but never commits afterwards, so nothing had established what the commit does
// once the window is broken.
//
// THE ARMS ARE ISOLATED, because the gate is an if/else-if chain and
// `values_synchronized` is tested BEFORE `trigger_ready`. A test that dropped the
// trigger AND let a divergent write through would be answered by the values arm
// and would report the trigger arm as covered when it never ran. So:
//
//   - the values arm DISABLES the trigger, writes, then RE-ENABLES it, leaving
//     the trigger ready and only the data diverged;
//   - the trigger arm drops the trigger and writes NOTHING, leaving the values
//     synchronized and only the trigger gone.
//
// Each refusal must carry its own reason, or the two arms are interchangeable and
// neither is attributed. And each is asserted on the DATABASE: the old column must
// still be there holding its value, because "refused" that still dropped the
// column would be the whole failure.
//
// The control commits an untouched window, without which every refusal here holds
// equally on a build where commits never work.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only - the online rename is
// PostgreSQL's.

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

const OWNER_APP = "app_commit_shape";
const RENAME_MIGRATION = "rename_display_name";

const CREATE = `import { table, t } from "zero-migrate";
export const name = "create_users";
export default {
  up() {
    table("users").create({
      columns: { id: t.int().notNull(), display_name: t.text() },
      primaryKey: ["id"],
    });
    table("users").insert({ rows: { id: 1, display_name: "ada" } });
  },
};
`;

const RENAME = `import { table, t } from "zero-migrate";
export const name = "${RENAME_MIGRATION}";
export default {
  up() {
    table("users").column("display_name").rename({ to: "full_name", type: t.text() });
  },
};
`;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "commitshape-"));
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

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ users: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_create_users.ts"), CREATE);
  writeFileSync(join(work, "migrations", "20260102000000_rename_display_name.ts"), RENAME);
  return work;
}

function runCli(
  work: string,
  schema: string,
  argv: string[],
): Promise<{ code: number | null; err: string }> {
  return new Promise((resolvePromise) => {
    const child = spawn(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, ...argv,
        "--dir", join(work, "migrations"),
        "--database-url", pgUrl(),
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--schema", schema,
        "--owner-app", OWNER_APP,
      ],
      { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
    );
    let err = "";
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("close", (code) =>
      resolvePromise({ code, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    );
  });
}

type Client = NonNullable<Awaited<ReturnType<typeof connectLivePg>>>;

/** Open the coexistence window and return the managed trigger's name. */
async function openWindow(client: Client, work: string, schema: string): Promise<string> {
  const applied = await runCli(work, schema, ["apply", "--approve"]);
  assert.equal(applied.code, 0, `apply must open the window; ${applied.err}`);
  const { rows } = await client.query(
    `SELECT tgname FROM pg_trigger t
       JOIN pg_class c ON c.oid = t.tgrelid
       JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = $1 AND c.relname = 'users' AND NOT t.tgisinternal`,
    [schema],
  );
  assert.equal(rows.length, 1, "the expand phase installs exactly one managed trigger");
  return rows[0].tgname as string;
}

/** Both columns of the single seeded row, as the DATABASE holds them. */
async function rowState(
  client: Client,
  schema: string,
): Promise<{ display_name: string | null; full_name: string | null }> {
  const { rows } = await client.query(
    `SELECT display_name, full_name FROM "${schema}".users WHERE id = 1`,
  );
  assert.equal(rows.length, 1, "the seeded row is present");
  return rows[0];
}

async function withWindow(
  ctx: Parameters<typeof connectLivePg>[0],
  prefix: string,
  body: (client: Client, work: string, schema: string, trigger: string) => Promise<void>,
): Promise<boolean> {
  const client = await connectLivePg(ctx);
  if (!client) return false;
  const schema = uniqueNamespace(prefix);
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const trigger = await openWindow(client, work, schema);
    await body(client, work, schema, trigger);
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
  return true;
}

test("the commit refuses a window whose columns have diverged, and keeps the old one", async (ctx) => {
  const ran = await withWindow(ctx, "shapeval", async (client, work, schema, trigger) => {
    // Diverge the DATA while leaving the trigger installed and enabled, so the
    // values arm is the only one that can answer. A write with the trigger live
    // would be mirrored and there would be nothing to detect.
    await client.query(`ALTER TABLE "${schema}".users DISABLE TRIGGER "${trigger}"`);
    await client.query(
      `UPDATE "${schema}".users SET display_name = 'ada-only-here' WHERE id = 1`,
    );
    await client.query(`ALTER TABLE "${schema}".users ENABLE TRIGGER "${trigger}"`);

    // The premise. Without this the refusal below could be about anything.
    const diverged = await rowState(client, schema);
    assert.equal(diverged.display_name, "ada-only-here");
    assert.equal(diverged.full_name, "ada", "only the old column moved");

    const committed = await runCli(work, schema, [
      "resolve", RENAME_MIGRATION, "--commit", "--approve",
    ]);
    assert.notEqual(committed.code, 0, "a diverged window must not commit");
    assert.match(
      committed.err,
      /no longer synchronized/,
      `the refusal must be attributed to the values check; got: ${committed.err}`,
    );

    // The point of refusing. The only correct copy is in the OLD column, and the
    // commit is what would have dropped it.
    const after = await rowState(client, schema);
    assert.equal(
      after.display_name,
      "ada-only-here",
      "the refused commit must not drop the column holding the surviving value",
    );
  });
  if (!ran) return;
});

test("the commit refuses a window whose dual-write trigger is gone", async (ctx) => {
  const ran = await withWindow(ctx, "shapetrg", async (client, work, schema, trigger) => {
    // Drop the trigger and write NOTHING. The values stay synchronized, so the
    // earlier arm of the chain cannot answer and only the trigger check can.
    await client.query(`DROP TRIGGER "${trigger}" ON "${schema}".users`);
    const untouched = await rowState(client, schema);
    assert.equal(untouched.display_name, "ada");
    assert.equal(untouched.full_name, "ada", "the values must still agree");

    const committed = await runCli(work, schema, [
      "resolve", RENAME_MIGRATION, "--commit", "--approve",
    ]);
    assert.notEqual(committed.code, 0, "a window with no dual-write trigger must not commit");
    assert.match(
      committed.err,
      /trigger is missing or disabled/,
      `the refusal must be attributed to the trigger check; got: ${committed.err}`,
    );
    assert.doesNotMatch(
      committed.err,
      /no longer synchronized/,
      "the values arm must not be the one answering here",
    );

    const after = await rowState(client, schema);
    assert.equal(after.display_name, "ada", "the refused commit must not drop the old column");
  });
  if (!ran) return;
});

test("the commit refuses a window whose trigger is merely DISABLED", async (ctx) => {
  // A disabled trigger is the quieter half: it is still in `pg_trigger`, so a
  // check that only asked whether the trigger EXISTS would pass it, and the window
  // would commit having mirrored nothing since the moment it was disabled.
  const ran = await withWindow(ctx, "shapedis", async (client, work, schema, trigger) => {
    await client.query(`ALTER TABLE "${schema}".users DISABLE TRIGGER "${trigger}"`);
    // Scoped to THIS schema's table. `tgname` alone is not unique across the
    // server: every fixture that opens a rename window on a `users` table gets the
    // same managed trigger name, so an unqualified lookup reads whichever schema
    // the catalog happens to return first.
    const { rows } = await client.query(
      `SELECT t.tgenabled FROM pg_trigger t
         JOIN pg_class c ON c.oid = t.tgrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = 'users' AND t.tgname = $2`,
      [schema, trigger],
    );
    assert.equal(rows.length, 1, "exactly this schema's trigger is inspected");
    assert.equal(rows[0].tgenabled, "D", "the trigger is present but disabled");

    const committed = await runCli(work, schema, [
      "resolve", RENAME_MIGRATION, "--commit", "--approve",
    ]);
    assert.notEqual(committed.code, 0, "a disabled trigger must not be treated as ready");
    assert.match(
      committed.err,
      /trigger is missing or disabled/,
      `the refusal must name the trigger check; got: ${committed.err}`,
    );
    const after = await rowState(client, schema);
    assert.equal(after.display_name, "ada", "the refused commit must not drop the old column");
  });
  if (!ran) return;
});

test("an untouched window still commits, so the three refusals mean something", async (ctx) => {
  const ran = await withWindow(ctx, "shapectl", async (client, work, schema) => {
    const committed = await runCli(work, schema, [
      "resolve", RENAME_MIGRATION, "--commit", "--approve",
    ]);
    assert.equal(committed.code, 0, `an intact window must commit; ${committed.err}`);
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      rows.map((row) => row.column_name),
      ["full_name", "id"],
      "the commit drops the old column when the window is intact",
    );
    const { rows: value } = await client.query(
      `SELECT full_name FROM "${schema}".users WHERE id = 1`,
    );
    assert.equal(value[0].full_name, "ada", "and the value survives the drop");
  });
  if (!ran) return;
});
