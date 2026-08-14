// A backfill whose cursor values are far above `u32::MAX` visits every row once.
//
// `docs/dialects.md` claims: "Integer and decimal cursor values remain exact across
// the JavaScript boundary." The existing backfill family
// (`backfill-cursor-ordering`, `-key-proof`, `-selective-cohort`, `-column-kind`)
// is thorough about ORDER, COHORT and COLUMN CHOICE, and uses small ids throughout.
// Nothing exercised a cursor whose MAGNITUDE is interesting.
//
// WHAT THIS DOES AND DOES NOT PROVE, stated because the obvious reading is wrong.
// The engine's contract is `|v| < 2^53`, and a double represents every integer
// below that exactly — so within the supported domain there is no rounding to lose.
// This is NOT a precision test and cannot be one.
//
// What it does exercise is REPRESENTATION, which is where the real defect in this
// class lived: F634 found that an integer above `u32::MAX` crossing napi as a JS
// value arrived as an f64 and was refused as "fractional", with `lint` passing the
// same migration. A cursor is the value most likely to make that fatal rather than
// merely loud, because it is written to the journal and read back between batches:
// a cursor that failed to round-trip would resume in the wrong place, skipping rows
// or reprocessing them.
//
// `batchSize: 1` is the point of the setup. It forces a cursor save and reload
// between EVERY row, so six rows mean six round-trips rather than one. The ids are
// six CONSECUTIVE values near 2^53, so a resume that landed even one position off
// would be visible immediately.
//
// The observable is a non-idempotent increment, not a flag: `n` must be exactly 1
// everywhere. A skipped row leaves 0 and a reprocessed row leaves 2, and neither
// shows up in an exit code or a row count.
//
// GATES: SQLite always runs; the PostgreSQL arm needs `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_bigcursor";
const TABLE = "cur_t";

/** Six CONSECUTIVE ids just under 2^53 — the top of the engine's exact-integer
 *  contract, and far above the `u32::MAX` cliff F634 was about. Consecutive so a
 *  resume landing one position off is immediately visible. */
const IDS = Array.from({ length: 6 }, (_, i) => (9007199254739960 + i).toString());

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "bigcursor-"));
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
  const rows = IDS.map((id) => `{ id: int64("${id}"), n: 0 }`).join(", ");
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t, int64 } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("${TABLE}").create({
      columns: { id: t.bigInt().notNull(), n: t.int().notNull() },
      primaryKey: ["id"],
    });
    table("${TABLE}").insert({ rows: [${rows}] });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `import { table } from "zero-migrate";
export const name = "b";
export default {
  data() {
    // batchSize 1: a cursor save and reload between every single row.
    table("${TABLE}").backfill({
      set: { n: (col) => col("n").add(1) },
      where: (col) => col("id").gt(0),
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 1,
    });
  },
  inverse() {
    table("${TABLE}").backfill({
      set: { n: (col) => col("n").sub(1) },
      where: (col) => col("id").gt(0),
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 1,
    });
  },
};
`,
  );
  return work;
}

function apply(
  work: string,
  databaseUrl: string,
  namespace: string | null,
): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", databaseUrl,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      ...(namespace ? ["--schema", namespace] : []),
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

/** `n` must be 1 on every row: 0 means the row was skipped, 2 means it was visited
 *  twice. Neither shows in an exit code or a row count. */
function assertVisitedExactlyOnce(rows: Array<{ id: string; n: number }>, where: string): void {
  assert.deepEqual(
    rows.map((row) => row.id),
    IDS,
    `${where}: every id must survive the round-trip exactly`,
  );
  assert.deepEqual(
    rows.map((row) => Number(row.n)),
    IDS.map(() => 1),
    `${where}: every row must be visited exactly once -- a cursor that failed to ` +
      `round-trip would resume in the wrong place, leaving 0 (skipped) or 2 (repeated)`,
  );
}

test("PostgreSQL backfills across a cursor far above u32::MAX", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("bigcur_pg");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = apply(work, pgUrl(), namespace);
    assert.equal(applied.code, 0, `the backfill must apply; ${applied.text}`);

    const { rows } = await client.query(
      `SELECT id::text AS id, n FROM "${namespace}"."${TABLE}" ORDER BY id`,
    );
    assertVisitedExactlyOnce(rows as Array<{ id: string; n: number }>, "PostgreSQL");
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("SQLite backfills across a cursor far above u32::MAX", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const applied = apply(work, `sqlite:${appPath}`, null);
    assert.equal(applied.code, 0, `the backfill must apply; ${applied.text}`);

    const db = new DatabaseSync(appPath, { readOnly: true });
    const rows = (
      db.prepare(`SELECT CAST(id AS TEXT) AS id, n FROM ${TABLE} ORDER BY id`).all() as Array<{
        id: string;
        n: number;
      }>
    ).map((row) => ({ id: String(row.id), n: Number(row.n) }));
    db.close();

    assertVisitedExactlyOnce(rows, "SQLite");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
