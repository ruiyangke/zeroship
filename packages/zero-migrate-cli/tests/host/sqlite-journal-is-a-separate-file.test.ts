// On SQLite the migration journal lives in its OWN database file, never in the
// application's.
//
// PostgreSQL and MySQL put the journal in a sibling SCHEMA, which is invisible to
// an application querying its own. SQLite has no schemas, so the same separation
// has to be a separate FILE -- and if it ever stopped being one, every user's
// application database would silently acquire `schema_migrations`,
// `schema_migrations_inflight` and `schema_migrations_supersedes` next to their own
// tables. Nothing would fail; the tables would simply be there, in backups, in
// `SELECT name FROM sqlite_master`, and in whatever the application enumerates.
//
// Two ways to choose the location, and both were exercised only through the Node
// API's explicit `journalPath`. The CLI's own paths -- the DERIVED default and the
// `--journal` flag -- had no end-to-end coverage:
//
//   default      `app.db`  ->  `app.migrations.db`   (`<name>.migrations.<ext>`)
//   explicit     `--journal <path>`
//
// THE ASSERTION IS EXCLUSIVE, not inclusive. Checking that the journal file
// contains the journal would pass just as well if the app database ALSO contained
// it, which is the failure this file exists to catch. So the app database is
// required to contain EXACTLY the authored table and nothing else.
//
// GATE: none. SQLite runs everywhere.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_journal";
const TABLE = "jr_t";

function project(): string {
  const work = mkdtempSync(join(HERE, "sqjournal-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "zero-migrate";
export const name = "a";
export default {
  schema() {
    table("${TABLE}").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

function apply(
  work: string,
  appPath: string,
  extra: readonly string[] = [],
): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", `sqlite:${appPath}`,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--owner-app", OWNER_APP,
      ...extra,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return { code: result.status, text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim() };
}

/** Every non-internal table in one SQLite file. `sqlite_sequence` is created by
 *  SQLite itself for AUTOINCREMENT and says nothing about who wrote the schema. */
function tablesIn(path: string): string[] {
  const db = new DatabaseSync(path, { readOnly: true });
  const rows = db
    .prepare(
      `SELECT name FROM sqlite_master
        WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY 1`,
    )
    .all() as Array<{ name: string }>;
  db.close();
  return rows.map((row) => row.name);
}

/** The journal's own tables, asserted by NAME so a file that merely exists but is
 *  empty cannot pass for a journal. */
const JOURNAL_TABLES = [
  "schema_migrations",
  "schema_migrations_inflight",
  "schema_migrations_supersedes",
];

test("the derived journal file holds the journal, and the app database does not", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const applied = apply(work, appPath);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    // `<name>.migrations.<ext>`, beside the application database.
    const journalPath = join(work, "app.migrations.db");
    assert.ok(
      existsSync(journalPath),
      `the derived journal file must exist at ${journalPath}`,
    );

    assert.deepEqual(
      tablesIn(appPath),
      [TABLE],
      "the application database must hold the authored table and NOTHING else -- " +
        "a journal that leaked into it would add its tables to every backup and " +
        "every sqlite_master listing the application makes",
    );
    assert.deepEqual(
      tablesIn(journalPath).filter((name) => JOURNAL_TABLES.includes(name)).sort(),
      [...JOURNAL_TABLES].sort(),
      "and the journal file must actually carry the journal, not merely exist",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("--journal relocates the journal and leaves the app database alone", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const journalPath = join(work, "custom-journal.db");
    const applied = apply(work, appPath, ["--journal", journalPath]);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    assert.ok(existsSync(journalPath), "the requested journal file must be created");
    // The derived default must NOT also appear: the flag replaces it rather than
    // adding a second journal.
    assert.equal(
      existsSync(join(work, "app.migrations.db")),
      false,
      "the override must replace the derived path, not sit alongside it",
    );

    assert.deepEqual(
      tablesIn(appPath),
      [TABLE],
      "the application database must still hold only the authored table",
    );
    assert.deepEqual(
      tablesIn(journalPath).filter((name) => JOURNAL_TABLES.includes(name)).sort(),
      [...JOURNAL_TABLES].sort(),
      "the journal must be in the file the operator named",
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
