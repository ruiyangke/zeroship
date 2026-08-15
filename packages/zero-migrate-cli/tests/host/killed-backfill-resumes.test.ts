// A backfill whose PROCESS is killed mid-flight resumes without losing or
// double-applying a row.
//
// `partial-deploy-resumes.test.ts` covers the FAILURE shape: a statement errors,
// its transaction rolls back, and re-running skips the completed steps. This is
// the other shape, and its mechanics are different: nothing errored. The process
// vanished while a windowed backfill was committing batch after batch, so the
// database holds partial work that NO journal event describes.
//
// Measured before this test existed, on live PostgreSQL:
//
//   after SIGKILL   15,450 / 200,000 rows filled; the journal held only the
//                   create. The backfill had no completed event at all.
//   re-run apply    200,000 / 200,000 filled, every one of them correct.
//
// WHY THIS IS WORTH PINNING. The cursor is what makes that true, and a cursor
// regression is invisible to every other test: a backfill that restarted from
// zero would still end with every row filled, and a backfill that resumed one
// window too far would leave a gap no count-based assertion notices. So the
// assertion is per-row correctness (`filled = val + 1`) over the WHOLE table,
// not a count.
//
// THE KILL IS TRIGGERED BY OBSERVED PROGRESS, not a timer. A fixed sleep either
// races the backfill to completion on a fast machine or wastes seconds on a slow
// one; polling until rows are actually filled makes the interruption real on
// both, which is the difference between this test measuring something and
// measuring nothing.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_killed_backfill";
const TABLE = "kb_t";
const ROWS = 20_000;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

const CREATE = `import { table, t } from "zero-migrate";
export const name = "kb_create";
export default {
  schema() {
    table("${TABLE}").create({
      columns: { id: t.int().notNull(), val: t.int(), filled: t.int() },
      primaryKey: ["id"],
    });
  },
};
`;

// batchSize 10 over 20k rows is 2000 windows — enough that the process is still
// working when the poll below sees its first committed batch.
const BACKFILL = `import { table } from "zero-migrate";
export const name = "kb_fill";
export default {
  data() {
    table("${TABLE}").backfill({
      name: "fill_kb",
      set: { filled: (col) => col("val").add(1) },
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 10,
    });
  },
  irreversible: "the prior filled value is not recorded",
};
`;

function project(): string {
  const work = mkdtempSync(join(HERE, "killbf-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(join(work, "migrations", "20260101000000_create.ts"), CREATE);
  return work;
}

function argv(work: string, schema: string): string[] {
  return [
    "--import", "tsx", CLI_BIN, "apply", "--approve",
    "--dir", join(work, "migrations"),
    "--database-url", pgUrl(),
    "--policy", join(work, "policy.toml"),
    "--registry", join(work, "registry.json"),
    "--schema", schema,
    "--owner-app", OWNER_APP,
  ];
}

const ENV = { ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" };

test("a backfill killed mid-flight resumes without losing or repeating a row", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("killbf");
  const work = project();
  const filled = async (): Promise<number> => {
    const { rows } = await client.query(
      `SELECT count(*)::int AS n FROM "${schema}"."${TABLE}" WHERE filled IS NOT NULL`,
    );
    return (rows[0] as { n: number }).n;
  };

  try {
    await client.query(`CREATE SCHEMA "${schema}"`);

    // The table and its rows must exist BEFORE the backfill migration does.
    // Authored the other way round, the backfill applies to an empty table,
    // journals itself complete, and can never run again - the setup mistake that
    // cost a full attempt when this was first measured by hand.
    assert.equal(
      spawnSync(process.execPath, argv(work, schema), {
        cwd: work,
        encoding: "utf8",
        env: { ...process.env, ...ENV },
      }).status,
      0,
      "the create must apply",
    );
    await client.query(
      `INSERT INTO "${schema}"."${TABLE}" (id, val)
       SELECT g, g FROM generate_series(1, ${ROWS}) g`,
    );
    writeFileSync(join(work, "migrations", "20260101000001_fill.ts"), BACKFILL);

    // Start the backfill and kill it once it has DEMONSTRABLY committed work.
    const child = spawn(process.execPath, argv(work, schema), {
      cwd: work,
      env: { ...process.env, ...ENV },
      stdio: "ignore",
    });
    const deadline = Date.now() + 60_000;
    let progressed = 0;
    while (Date.now() < deadline) {
      await new Promise((done) => setTimeout(done, 100));
      progressed = await filled();
      if (progressed > 0) break;
    }
    child.kill("SIGKILL");
    await new Promise((done) => child.on("exit", done));

    assert.ok(
      progressed > 0,
      "the backfill must have committed at least one batch, or the kill proved nothing",
    );
    assert.ok(
      progressed < ROWS,
      `the kill must land MID-backfill; it finished first (${progressed}/${ROWS}), so ` +
        `this run measured a completed apply rather than an interrupted one`,
    );

    // A partial with no journal event for the backfill: the shape a failed
    // statement never produces.
    const { rows: journal } = await client.query(
      `SELECT name FROM "${schema}_migrations".schema_migrations
        WHERE event_kind = 'applied'`,
    );
    assert.equal(journal.length, 1, "only the create is journaled at this point");

    // The retry.
    assert.equal(
      spawnSync(process.execPath, argv(work, schema), {
        cwd: work,
        encoding: "utf8",
        env: { ...process.env, ...ENV },
      }).status,
      0,
      "re-running apply must finish the interrupted backfill",
    );

    // Per ROW, not per count. A restart-from-zero and a resume both end at ROWS
    // filled; only the values distinguish a correct resume from a cursor that
    // skipped a window.
    const { rows: check } = await client.query(
      `SELECT count(*)::int AS total,
              count(*) FILTER (WHERE filled = val + 1)::int AS correct
         FROM "${schema}"."${TABLE}"`,
    );
    const { total, correct } = check[0] as { total: number; correct: number };
    assert.equal(total, ROWS, "every row is still present");
    assert.equal(
      correct,
      ROWS,
      `every row must carry its own computed value; a cursor that skipped or ` +
        `repeated a window shows up here and nowhere else (${correct}/${total})`,
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
