// SQLite apply driven through the shipped CLI binary, end to end.
//
// `docs/getting-started.md` and `docs/cli.md` both advertise it - "SQLite apply is
// available through Node, the CLI, and Rust" - and `TODO.md` listed it as an open
// capability gap, "SQLite apply from Node/CLI (currently Rust-only)". One of those
// was stale, and nothing in this suite could say which: the only SQLite references
// here parse DSN strings (`driverFor`, `hasInlinePassword`) and never reach a
// database.
//
// It works, so the TODO entry was the stale one. This arm exists so the answer stops
// depending on which document you read.
//
// It also fills the gap that let the disagreement persist. The Rust side proves the
// SQLite backend (`crates/zero-migrate-node/tests/rollback_sqlite.rs` and the
// in-crate suites), and the host suite proves the CLI against PostgreSQL and MySQL,
// but no arm ran the CLI against SQLite - the one combination both claims are about.
//
// GATE: none. SQLite is an in-process file, so unlike every other arm here this one
// runs everywhere, including a checkout with no database containers up.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_sqlite_cli";

function spawnCli(args: readonly string[], cwd: string) {
  return spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...args], {
    encoding: "utf8",
    cwd,
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
}

/** The charter the walkthrough uses, scoped to SQLite's `main` schema. Every knob is
 *  default-deny, so the grants are what make the schema ownable at all. */
const CHARTER = `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["main"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["main"] }
`;

const MIGRATION = `import { table, t } from "zero-migrate";

export const name = "create_notes";

export default {
  up() {
    table("notes").create({
      columns: {
        id: t.int().notNull(),
        body: t.string({ length: 64 }).notNull(),
      },
      primaryKey: ["id"],
    });
    table("notes").insert({ rows: { id: 1, body: "written by the CLI" } });
  },
};
`;

test("the CLI applies to a SQLite file and the rows are really there", () => {
  // Inside the test directory, not the system temp dir: the migration imports
  // "zero-migrate", which only resolves from within the workspace. That is a
  // property of the unpublished source checkout, not a defect.
  const work = mkdtempSync(join(HERE, "sqlite-cli-"));
  const dbPath = join(work, "app.db");
  try {
    const migrations = join(work, "migrations");
    writeFileSync(join(work, "policy.toml"), CHARTER);
    mkdirSync(migrations);
    writeFileSync(join(migrations, "20260101000000_create_notes.ts"), MIGRATION);

    const applied = spawnCli(
      [
        "apply",
        "--dir",
        migrations,
        "--database-url",
        `sqlite:${dbPath}`,
        "--policy",
        join(work, "policy.toml"),
        "--schema",
        "main",
        "--owner-app",
        OWNER_APP,
      ],
      work,
    );

    assert.equal(
      applied.status,
      0,
      `apply must succeed; stdout=${applied.stdout} stderr=${applied.stderr}`,
    );

    // Read the FILE back rather than trusting the reply. The reply is the engine
    // describing its own work; the point of this arm is that a SQLite database on
    // disk received it.
    const db = new DatabaseSync(dbPath);
    try {
      const tables = db
        .prepare("SELECT name FROM sqlite_master WHERE type = ?")
        .all("table")
        .map((row: Record<string, unknown>) => row.name as string);
      assert.ok(tables.includes("notes"), `the table exists; saw ${tables.join(",")}`);

      // `node:sqlite` returns null-prototype rows, so compare fields rather than
      // deepEqual against an object literal.
      const rows = db.prepare("SELECT id, body FROM notes ORDER BY id").all() as Array<
        Record<string, unknown>
      >;
      assert.equal(rows.length, 1, "one row landed");
      assert.equal(rows[0]?.id, 1, "the authored id landed in the file");
      assert.equal(rows[0]?.body, "written by the CLI", "the authored body landed in the file");
    } finally {
      db.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
