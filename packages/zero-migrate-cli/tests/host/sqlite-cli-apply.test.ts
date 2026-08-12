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
// The second arm covers the one thing zero-migrate does to a SQLite database that
// OUTLIVES the apply, and so is the one an operator has to be told about in advance.
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

// A WAL application database comes back from an apply in DELETE journal mode, and
// stays there.
//
// `docs/security-model.md` warns about it in one sentence: "Opening an application
// database that uses WAL changes its persistent journal mode." That is the only
// effect zero-migrate has on a SQLite database that OUTLIVES the migration. It is
// not a side effect of the schema change - it is a property of the connection
// profile, so it lands even on an apply that changes nothing, and it does not
// revert when the connection closes.
//
// It is deliberate. A transaction spanning `main` and the attached `_mig` journal
// is crash-atomic only under SQLite's super-journal protocol, which WAL, MEMORY,
// and OFF do not provide, so `enforce_atomic_profile_for_schema` pins DELETE and
// `synchronous = FULL` and reads both back rather than trusting the assignment.
//
// Nothing measured it. `journal_mode` appeared in the whole test tree only as a
// PRAGMA the authorizer denies. So the promise an operator plans around - "your WAL
// database will not be WAL afterwards" - rested on one sentence of prose, and the
// refusal path beside it ("remained {actual}") had no shape either.
//
// The assertions read the FILE with a separate connection after the CLI process has
// exited. A pragma read on the engine's own connection would only be the engine
// agreeing with itself, and the claim is specifically about persistence.

test("a WAL application database is left in DELETE journal mode, persistently", () => {
  const work = mkdtempSync(join(HERE, "sqlite-wal-"));
  const dbPath = join(work, "app.db");
  try {
    // Set up a genuinely WAL database, and prove the setup took before measuring
    // what the apply did to it. Without this the arm below would also pass on a
    // database that was never WAL in the first place.
    const before = new DatabaseSync(dbPath);
    try {
      before.exec("PRAGMA journal_mode = WAL");
      before.exec("CREATE TABLE wal_probe (id INTEGER PRIMARY KEY)");
    } finally {
      before.close();
    }
    const reopened = new DatabaseSync(dbPath);
    try {
      const mode = reopened.prepare("PRAGMA journal_mode").get() as Record<string, unknown>;
      assert.equal(
        String(mode?.journal_mode).toLowerCase(),
        "wal",
        "the fixture must start from a database that really is in WAL mode",
      );
    } finally {
      reopened.close();
    }

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
      `apply against a WAL database must succeed; stdout=${applied.stdout} stderr=${applied.stderr}`,
    );

    // A fresh connection, after the CLI process exited. Journal mode is stored in
    // the database header, so this is the persistence claim itself.
    const after = new DatabaseSync(dbPath);
    try {
      const mode = after.prepare("PRAGMA journal_mode").get() as Record<string, unknown>;
      assert.equal(
        String(mode?.journal_mode).toLowerCase(),
        "delete",
        "the apply must leave the application database in DELETE journal mode",
      );

      // The control against a vacuous pass: the apply has to have actually run.
      // A CLI that exited 0 without touching the database would satisfy the
      // journal-mode assertion only by leaving WAL alone, but this catches the
      // reverse mistake - a migration that never landed.
      const rows = after.prepare("SELECT id, body FROM notes ORDER BY id").all() as Array<
        Record<string, unknown>
      >;
      assert.equal(rows.length, 1, "the migration really applied to the WAL database");
      assert.equal(rows[0]?.body, "written by the CLI", "and wrote its authored row");
    } finally {
      after.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

// `plan` does it too, and `plan` is advertised as a dry run.
//
// This arm exists because the previous one made the behaviour look like a property
// of applying, and it is not. `docs/cli.md` calls `plan` "a live dry run" that "does
// not call the apply or resolution APIs and does not execute the rendered migration
// SQL", and says "planning uses a read-only status path". All of that is true of the
// SQL. None of it is true of the FILE: `statusIrSqlite` opens through the same
// `SqliteBackend::open`, so `plan` gets the same hardened profile as apply and pins
// `journal_mode = DELETE` on the application database before it reads anything. The
// `readOnly` flag reaches only the journal-bootstrap decision, not the connection.
//
// So a preview command permanently changes an operator's database, silently: WAL is
// what gives SQLite concurrent readers alongside a writer, DELETE serializes them,
// and the `-wal` sidecar is removed on the way. Running `plan` from CI against a
// live SQLite database degrades that database's concurrency and does not say so.
//
// THIS ARM IS WRITTEN TO FAIL WHEN THAT IMPROVES, not to bless it. The fix is a
// genuinely read-only open for the status path - one that does not pin the atomic
// profile, since nothing on that path commits across `main` and `_mig` - and it is
// deliberately not attempted here: it changes the hardened connection used by every
// SQLite entry point, and that decision wants more than a passing test. Until then,
// `docs/cli.md` carries the warning this measured, and this is where the current
// behaviour is recorded rather than assumed.

test("plan, a dry run, converts a WAL application database too - the documented wart", () => {
  const work = mkdtempSync(join(HERE, "sqlite-plan-wal-"));
  const dbPath = join(work, "app.db");
  try {
    const before = new DatabaseSync(dbPath);
    try {
      before.exec("PRAGMA journal_mode = WAL");
      before.exec("CREATE TABLE wal_probe (id INTEGER PRIMARY KEY)");
    } finally {
      before.close();
    }

    const migrations = join(work, "migrations");
    writeFileSync(join(work, "policy.toml"), CHARTER);
    mkdirSync(migrations);
    writeFileSync(join(migrations, "20260101000000_create_notes.ts"), MIGRATION);

    const planned = spawnCli(
      [
        "plan",
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
      planned.status,
      0,
      `plan must succeed; stdout=${planned.stdout} stderr=${planned.stderr}`,
    );
    // The control on the other side: plan really did preview the pending work, so
    // the journal-mode change below is not the artefact of a command that bailed.
    assert.match(
      planned.stdout,
      /would apply 1 migration/,
      "plan must have previewed the pending migration",
    );

    const after = new DatabaseSync(dbPath);
    try {
      const mode = after.prepare("PRAGMA journal_mode").get() as Record<string, unknown>;
      assert.equal(
        String(mode?.journal_mode).toLowerCase(),
        "delete",
        "TODAY plan converts a WAL database; when a read-only open lands this must read 'wal'",
      );

      // And it really was only a preview - nothing was applied. Without this the
      // arm could not tell "plan previewed and converted" from "plan applied".
      const applied = after
        .prepare("SELECT name FROM sqlite_master WHERE type = ? AND name = ?")
        .all("table", "notes");
      assert.equal(applied.length, 0, "plan must not have created the previewed table");
    } finally {
      after.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
