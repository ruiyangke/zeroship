// The coexistence window while the expand backfill is still running.
//
// `rename-dual-write-window.test.ts` proves the trigger mirrors writes, but it
// writes to a settled table. The real rollout is noisier: the expand phase runs a
// BACKFILL copying every existing row into the new column, and the application
// keeps writing the whole time. Two writers, same rows.
//
// That is a lost-update shape. The backfill reads the old column and writes the
// new one; a concurrent write changes the old column and the trigger writes the
// new one. If a stale backfill batch lands after the trigger, it overwrites a
// value the application already committed - and the commit phase then DROPS the
// old column, destroying the only correct copy. Silent, and unrecoverable.
//
// THE OVERLAP IS ASSERTED, NOT HOPED FOR. A test that wrote before the backfill
// started, or after it finished, would report zero lost updates and mean nothing
// - which is the failure mode of most concurrency tests. So the write loop also
// samples `schema_backfills`, counts how many of its writes landed while the
// backfill was genuinely mid-flight (`0 < rows_done < N` and not complete), and
// requires that count to be substantial. If the machine is too fast for the
// windows to overlap, this fails loudly rather than passing empty.
//
// Measured on a live server: ~2,900 concurrent writes, ~2,700 of them with the
// backfill in flight, max `rows_done` within 20 rows of the end, zero lost
// updates.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. PostgreSQL only.

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

const OWNER_APP = "app_rename_concurrent";
/** Large enough that the backfill spans the write loop rather than beating it. */
const ROW_COUNT = 120_000;
/** Below this, the overlap was not real and the result would be meaningless. */
const MIN_IN_FLIGHT_SAMPLES = 25;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

const CREATE = `import { table, t } from "zero-migrate";
export const name = "create_users";
export default {
  up() {
    table("users").create({
      columns: { id: t.int().notNull(), display_name: t.text() },
      primaryKey: ["id"],
    });
  },
};
`;

const RENAME = `import { table, t } from "zero-migrate";
export const name = "rename_display_name";
export default {
  up() {
    table("users").column("display_name").rename({ to: "full_name", type: t.text() });
  },
};
`;

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "renameconc-"));
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
  return work;
}

function startApply(work: string, schema: string): {
  done: Promise<{ code: number | null; err: string }>;
} {
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
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
  return {
    done: new Promise((resolvePromise) =>
      child.on("close", (code) =>
        resolvePromise({ code, err: err.replace(/^WARNING.*$/gm, "").trim() }),
      ),
    ),
  };
}

test("a write committed during the expand backfill is not overwritten by it", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const writer = await connectLivePg(ctx);
  if (!writer) return;
  const sampler = await connectLivePg(ctx);
  if (!sampler) return;

  const schema = uniqueNamespace("renameconc");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    assert.equal((await startApply(work, schema).done).code, 0, "the table must be created");

    await client.query(
      `INSERT INTO "${schema}".users (id, display_name)
         SELECT g, 'orig' || g FROM generate_series(1, ${ROW_COUNT}) g`,
    );
    writeFileSync(join(work, "migrations", "20260102000000_rename_display_name.ts"), RENAME);

    // The rename runs in one process while this one writes through the OLD column.
    const rename = startApply(work, schema);
    let running = true;
    void rename.done.then(() => {
      running = false;
    });

    const written = new Map<number, string>();
    let inFlightSamples = 0;
    let maxRowsDone = -1;
    let next = 1;
    while (running) {
      const target = next;
      next = (next % ROW_COUNT) + 1;
      const value = `W${target}`;
      try {
        await writer.query(`UPDATE "${schema}".users SET display_name = $1 WHERE id = $2`, [
          value,
          target,
        ]);
        written.set(target, value);
      } catch {
        // The table is briefly locked while the expand DDL runs; those writes
        // simply do not happen, and an unrecorded write is not a lost one.
      }
      try {
        const { rows } = await sampler.query(
          `SELECT rows_done::bigint AS d, complete FROM "${meta}".schema_backfills`,
        );
        if (rows.length === 1) {
          const done = Number(rows[0].d);
          if (done > maxRowsDone) maxRowsDone = done;
          if (!rows[0].complete && done > 0 && done < ROW_COUNT) inFlightSamples += 1;
        }
      } catch {
        // The journal table does not exist until the backfill registers.
      }
    }

    const outcome = await rename.done;
    assert.equal(outcome.code, 0, `the rename must succeed; ${outcome.err}`);

    // The overlap evidence. Without this the zero below means nothing: it would
    // hold just as well for writes that all landed after the backfill finished.
    assert.ok(
      inFlightSamples >= MIN_IN_FLIGHT_SAMPLES,
      `the writes must overlap a backfill that is genuinely mid-flight; ` +
        `saw ${inFlightSamples} in-flight samples, max rows_done ${maxRowsDone}, ` +
        `${written.size} writes`,
    );

    // The claim: every value this process committed is the value that survived.
    const ids = [...written.keys()];
    const { rows } = await client.query(
      `SELECT id, display_name, full_name FROM "${schema}".users WHERE id = ANY($1::int[])`,
      [ids],
    );
    const lost = rows
      .filter((row) => row.full_name !== written.get(Number(row.id)))
      .map((row) => ({
        id: Number(row.id),
        old: row.display_name,
        got: row.full_name,
        expected: written.get(Number(row.id)),
      }));
    assert.deepEqual(
      lost,
      [],
      "a stale backfill batch must not overwrite a value the application committed",
    );
    assert.equal(
      rows.length,
      ids.length,
      "every row written during the window must still be present",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await sampler.end().catch(() => {});
    await writer.end().catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
