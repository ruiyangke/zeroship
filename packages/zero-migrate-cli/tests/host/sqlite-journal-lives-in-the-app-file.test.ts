// On SQLite the migration journal lives in the database file it describes, under
// the `__zeroship_schema_` name fence. That file also carries the application's own
// tables and the platform data-plane ones, including the unmask audit log.
//
// PostgreSQL and MySQL put the journal in the tenant's own schema behind the same
// prefix. SQLite has no schemas, so the prefix is the whole of the separation, and
// what it separates is visible: without it a creator declaring a table called
// `schema_migrations` would have it silently adopted as the journal, and the
// journal's own tables would be indistinguishable from the creator's in a backup,
// in `SELECT name FROM sqlite_master`, and in whatever the application enumerates.
//
// So the fence is asserted in BOTH directions. An arm that only checked that the
// journal's tables are fenced would pass for a fence that swallowed everything;
// an arm that only checked a creator table's visibility would pass for no fence at
// all.
//
// The application table set is asserted EXACTLY, so neither a journal leak nor a
// missing creator table can hide inside a subset check.
//
// GATE: none. SQLite runs everywhere.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// The host suite builds and resolves its addon in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zeroship-migrate-node/zeroship-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_journal";
const TABLE = "jr_t";
// A creator table whose name is the one the fence exists to protect, declared
// alongside an ordinary one. It is a legal collection name: the reserved prefix is
// `__zeroship`, and this is not it.
const LOOKALIKE = "schema_migrations";

/** The journal's own tables, by name, so an empty fence cannot pass for a journal. */
const JOURNAL_TABLES = [
  "__zeroship_schema_migrations",
  "__zeroship_schema_migrations_inflight",
  "__zeroship_schema_migrations_supersedes",
];

/** What the application database must hold, exactly. */
const APP_TABLES = ["__zeroship_audit_unmask", TABLE, LOOKALIKE].sort();

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
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ [TABLE]: OWNER_APP, [LOOKALIKE]: OWNER_APP }),
  );
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t } from "@zeroship/migrate";
export const name = "a";
export default {
  schema() {
    table("${TABLE}").create({ columns: { id: t.int().required() }, primaryKey: ["id"] });
    table("${LOOKALIKE}").create({ columns: { id: t.int().required() }, primaryKey: ["id"] });
  },
};
`,
  );
  return work;
}

function apply(work: string, appPath: string): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", `sqlite:${appPath}`,
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--owner-app", OWNER_APP,
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

test("the journal and the application's tables share one file, told apart by the fence", () => {
  const work = project();
  try {
    const appPath = join(work, "app.db");
    const applied = apply(work, appPath);
    assert.equal(applied.code, 0, `the migration must apply; ${applied.text}`);

    const present = tablesIn(appPath);

    // The journal is really there, and it is really in THIS file.
    assert.deepEqual(
      present.filter((name) => JOURNAL_TABLES.includes(name)).sort(),
      [...JOURNAL_TABLES].sort(),
      "the journal's tables must be in the database they describe",
    );

    // And so is the creator's whole set, including the name the fence protects.
    assert.deepEqual(
      present.filter((name) => !name.startsWith("__zeroship_schema_")).sort(),
      APP_TABLES,
      "the application's own tables must all be present, and `schema_migrations` must " +
        "still be the creator's rather than adopted as the journal",
    );

    // The lookalike is a table of its own, not the journal wearing another name:
    // it has the shape the migration declared, which the journal does not.
    const db = new DatabaseSync(appPath, { readOnly: true });
    try {
      const columns = (
        db.prepare(`PRAGMA table_info("${LOOKALIKE}")`).all() as Array<{ name: string }>
      ).map((row) => row.name);
      assert.deepEqual(
        columns,
        ["id"],
        "the creator's `schema_migrations` must carry the creator's declared column",
      );
    } finally {
      db.close();
    }

    // Nothing beside the database file: the journal has no sibling of its own.
    const strays = tablesIn(appPath).length;
    assert.ok(strays > 0, "the assertions above ran over a non-empty catalog");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
