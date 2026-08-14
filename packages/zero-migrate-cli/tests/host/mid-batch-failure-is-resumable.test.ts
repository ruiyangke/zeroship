// A deploy that fails part-way is PARTIAL and RESUMABLE, and the journal says which.
//
// `OnUnmet::Halt` states the contract in the engine's own words:
//
//   > this stops the batch going FORWARD - no later-in-order migration is applied
//   > after the failing one. It does NOT undo migrations already committed earlier
//   > in the same batch: each migration commits independently (per-migration
//   > commit), so the migrations that succeeded before the halt stay applied. Halt
//   > is fail-forward-stop, not a batch-wide rollback.
//
// That is an operational promise, not an implementation detail. An operator whose
// deploy exits 1 needs to know the schema is half-migrated, WHICH half, and that
// re-running finishes the job rather than starting over or double-applying.
//
// `mysql-partial-ddl-recovery.test.ts` covers the INTRA-migration case -- one
// migration whose second statement fails after the first auto-committed. This is
// the INTER-migration case, which is a different mechanism (per-migration commit
// and the journal's pending computation) and applies to every dialect rather than
// only the one without transactional DDL.
//
// THE RESUMPTION ASSERTION IS THE SHARP ONE. After the obstruction is cleared and
// apply re-runs, the journal must hold EXACTLY ONE applied row per migration. That
// is what proves the already-applied migration was SKIPPED rather than re-run: a
// re-run would leave two rows for it, and on a migration carrying data rather than
// a bare `create` it would apply its effect twice. Counting rows catches that where
// "the tables all exist" would not.
//
// The obstruction is a table created out of band, so the failure is the engine's
// own refusal rather than an injected fault. It is caught by the offline FOLD
// (`failed to project pending schema after envelope "b"`), before the backend opens
// anything -- which is itself worth knowing: the batch stops without the failing
// migration having touched the database at all.
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

const OWNER_APP = "app_midbatch";
const TABLES = ["b_a", "b_b", "b_c"] as const;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** Three migrations, one table each, in authored order. The middle one is the one
 *  that will collide with a table created out of band. */
function project(): string {
  const work = mkdtempSync(join(HERE, "midbatch-"));
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
`,
  );
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify(Object.fromEntries(TABLES.map((table) => [table, OWNER_APP]))),
  );
  TABLES.forEach((table, index) => {
    const stamp = `2026010${index + 1}000000`;
    writeFileSync(
      join(work, "migrations", `${stamp}_${table}.ts`),
      `import { table, t } from "zero-migrate";
export const name = "${table}";
export default {
  up() {
    table("${table}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
    );
  });
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

test("a mid-batch failure leaves the earlier migration applied and resumes cleanly", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("midbatch");
  const work = project();

  const tables = async (): Promise<string[]> =>
    (
      await client.query(
        `SELECT table_name FROM information_schema.tables
          WHERE table_schema = $1 AND table_name LIKE 'b\\_%' ORDER BY 1`,
        [namespace],
      )
    ).rows.map((row: { table_name: string }) => row.table_name);
  const appliedNames = async (): Promise<string[]> =>
    (
      await client.query(
        `SELECT name FROM "${namespace}_migrations".schema_migrations
          WHERE event_kind = 'applied' ORDER BY event_seq`,
      )
    ).rows.map((row: { name: string }) => row.name);

  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    // The obstruction: the MIDDLE migration's table already exists, with a
    // different shape, created by something outside this migration set.
    await client.query(`CREATE TABLE "${namespace}".b_b (other int)`);

    const halted = apply(work, pgUrl(), namespace);
    assert.equal(halted.code, 1, `the obstructed deploy must fail; ${halted.text}`);

    assert.deepEqual(
      await tables(),
      ["b_a", "b_b"],
      "the FIRST migration must stay applied (per-migration commit) and the THIRD " +
        "must not have run -- b_b here is the out-of-band table, not the migration's",
    );
    assert.deepEqual(
      await appliedNames(),
      ["create_table_b_a"],
      "only the migration that succeeded may be journaled",
    );

    // Clear the obstruction and re-run: the deploy must FINISH, not restart.
    await client.query(`DROP TABLE "${namespace}".b_b`);
    const resumed = apply(work, pgUrl(), namespace);
    assert.equal(resumed.code, 0, `the resumed deploy must succeed; ${resumed.text}`);

    assert.deepEqual(await tables(), ["b_a", "b_b", "b_c"], "every table must now exist");
    // The sharp one: exactly one row per migration. Two rows for `b_a` would mean
    // it was re-applied rather than skipped, which for a data migration would
    // double its effect.
    assert.deepEqual(
      await appliedNames(),
      ["create_table_b_a", "create_table_b_b", "create_table_b_c"],
      "the already-applied migration must be SKIPPED on resume, not re-applied -- " +
        "a second row for it is what a re-run would leave",
    );
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

test("SQLite halts and resumes the same way", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const journalPath = join(work, "app.migrations.db");

    // Create the obstruction before any migration runs.
    const seed = new DatabaseSync(appPath);
    seed.exec(`CREATE TABLE b_b (other INTEGER)`);
    seed.close();

    const halted = apply(work, `sqlite:${appPath}`, null);
    assert.equal(halted.code, 1, `the obstructed deploy must fail; ${halted.text}`);

    const readNames = (): string[] => {
      const db = new DatabaseSync(journalPath, { readOnly: true });
      const rows = db
        .prepare(
          `SELECT name FROM schema_migrations WHERE event_kind = 'applied' ORDER BY event_seq`,
        )
        .all() as Array<{ name: string }>;
      db.close();
      return rows.map((row) => row.name);
    };
    const readTables = (): string[] => {
      const db = new DatabaseSync(appPath, { readOnly: true });
      const rows = db
        .prepare(`SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'b\\_%' ESCAPE '\\' ORDER BY 1`)
        .all() as Array<{ name: string }>;
      db.close();
      return rows.map((row) => row.name);
    };

    assert.deepEqual(readTables(), ["b_a", "b_b"], "the first migration must stay applied");
    assert.deepEqual(readNames(), ["create_table_b_a"], "only it may be journaled");

    const drop = new DatabaseSync(appPath);
    drop.exec(`DROP TABLE b_b`);
    drop.close();

    const resumed = apply(work, `sqlite:${appPath}`, null);
    assert.equal(resumed.code, 0, `the resumed deploy must succeed; ${resumed.text}`);
    assert.deepEqual(readTables(), ["b_a", "b_b", "b_c"]);
    assert.deepEqual(
      readNames(),
      ["create_table_b_a", "create_table_b_b", "create_table_b_c"],
      "exactly one applied row per migration -- the first was skipped, not re-run",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
