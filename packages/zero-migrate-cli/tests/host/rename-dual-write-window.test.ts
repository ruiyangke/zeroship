// The coexistence window, from the application's side.
//
// A PostgreSQL online rename runs in two deploys. The expand phase leaves the old
// and new columns side by side and installs a dual-write trigger, and the whole
// point of that window is that a RUNNING APPLICATION keeps working through it:
// old instances still write the old column, new instances write the new one, and
// neither loses anything.
//
// `resolve-lifecycle.test.ts` covers the window's TRANSITIONS - open it, commit
// it, roll it back - and asserts the values survive each one. It never writes to
// the table while the window is open, which is the only thing the window exists
// for. So the trigger that makes coexistence work had nothing exercising it.
//
// That gap matters more than an ordinary one because of F460: drift detection
// hardcodes triggers as empty on the live side, so a dual-write trigger that was
// never installed, or was installed and then dropped, is invisible to every
// structural check this engine has. Nothing would notice until an application
// wrote through the old column and the new one silently stayed NULL - and the
// commit phase would then drop the old column, taking the only copy with it.
//
// THREE WRITE SHAPES, because an application mid-rollout produces all three:
//
//   - an old-code INSERT naming only the old column;
//   - an old-code UPDATE of the old column on a row that predates the window;
//   - a new-code INSERT naming only the new column.
//
// Each is asserted from the OTHER side, and then the window is committed so the
// rows are read once more through the surviving column. A trigger that mirrored
// on INSERT but not UPDATE, or in one direction only, fails a specific arm rather
// than the whole file.
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

const OWNER_APP = "app_rename_dual_write";
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
  const work = mkdtempSync(join(HERE, "dualwrite-"));
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

test("writes through either column reach the other while the rename window is open", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("dualwrite");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const applied = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `apply must open the window; ${applied.err}`);

    // The premise: both columns are present, so the writes below are ambiguous
    // in exactly the way a mid-rollout application makes them.
    const { rows: columns } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users' ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      columns.map((row) => row.column_name),
      ["display_name", "full_name", "id"],
      "the expand phase must leave both columns present",
    );

    // 1. Old-code INSERT: names only the old column.
    await client.query(
      `INSERT INTO "${schema}".users (id, display_name) VALUES (2, 'grace')`,
    );
    // 2. Old-code UPDATE: touches a row that predates the window.
    await client.query(
      `UPDATE "${schema}".users SET display_name = 'ada-updated' WHERE id = 1`,
    );
    // 3. New-code INSERT: names only the new column.
    await client.query(`INSERT INTO "${schema}".users (id, full_name) VALUES (3, 'hopper')`);

    const { rows: mirrored } = await client.query(
      `SELECT id, display_name, full_name FROM "${schema}".users ORDER BY id`,
    );
    const byId = new Map(mirrored.map((row) => [Number(row.id), row]));

    assert.equal(
      byId.get(2)?.full_name,
      "grace",
      "an INSERT naming only the old column must reach the new one",
    );
    assert.equal(
      byId.get(1)?.full_name,
      "ada-updated",
      "an UPDATE of the old column must reach the new one, including on a pre-window row",
    );
    assert.equal(
      byId.get(3)?.display_name,
      "hopper",
      "an INSERT naming only the new column must reach the old one",
    );

    // And the window closes without losing any of it. The commit drops the old
    // column, so anything the trigger failed to mirror is gone for good - which
    // is why this is asserted after the drop rather than only before it.
    const committed = await runCli(work, schema, [
      "resolve", RENAME_MIGRATION, "--commit", "--approve",
    ]);
    assert.equal(committed.code, 0, `the commit must succeed; ${committed.err}`);

    const { rows: after } = await client.query(
      `SELECT id, full_name FROM "${schema}".users ORDER BY id`,
    );
    assert.deepEqual(
      after.map((row) => [Number(row.id), row.full_name]),
      [
        [1, "ada-updated"],
        [2, "grace"],
        [3, "hopper"],
      ],
      "every value written during the window must survive the commit",
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

test("the dual-write trigger is really what carries it, not a default or a copy at commit", async (ctx) => {
  // The control. Every assertion above would also hold if the values happened to
  // arrive some other way - a column default, or a bulk copy performed by the
  // commit step. Dropping the trigger and repeating one write separates those:
  // with the trigger gone the mirror must NOT happen, which is the only thing
  // that proves the trigger was doing the work.
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("dualctl");
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const applied = await runCli(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `apply must open the window; ${applied.err}`);

    const { rows: triggers } = await client.query(
      `SELECT tgname FROM pg_trigger t
         JOIN pg_class c ON c.oid = t.tgrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = 'users' AND NOT t.tgisinternal`,
      [schema],
    );
    assert.ok(
      triggers.length > 0,
      "the expand phase must install a trigger on the renamed table",
    );

    for (const row of triggers) {
      await client.query(
        `DROP TRIGGER "${row.tgname}" ON "${schema}".users`,
      );
    }
    await client.query(
      `INSERT INTO "${schema}".users (id, display_name) VALUES (9, 'orphan')`,
    );
    const { rows } = await client.query(
      `SELECT full_name FROM "${schema}".users WHERE id = 9`,
    );
    assert.equal(
      rows[0]?.full_name,
      null,
      "with the trigger dropped the mirror must stop, or the test above proved nothing",
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
