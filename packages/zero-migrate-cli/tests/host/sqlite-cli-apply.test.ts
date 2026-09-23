// SQLite apply and rollback driven through the shipped CLI binary, end to end.
//
// The suite's other SQLite references parse DSN strings (`driverFor`,
// `hasInlinePassword`) and never reach a database; these arms drive a real
// apply and a real unwind so support for the combination does not rest on prose.
//
// It also fills the coverage gap directly. The Rust side proves the
// SQLite backend (`crates/zeroship-migrate-node/tests/rollback_sqlite.rs` and the
// in-crate suites), and the host suite proves the CLI against PostgreSQL and
// MySQL; these arms run the CLI against SQLite. The rollback arm is the only place
// the facade's SQLite branch of `rollback()` is driven end to end - every other
// `rollback()` arm in this suite opens a network session - so it is what keeps the
// request that branch assembles honest.
//
// The WAL arm covers the one thing zero-migrate does to a SQLite database that
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
  `../../../../crates/zeroship-migrate-node/zeroship-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
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

const SCHEMA_MIGRATION = `import { table, t } from "@zeroship/migrate";

export const name = "create_notes";

export default {
  schema() {
    table("notes").create({
      columns: {
        id: t.int().required(),
        body: t.string({ length: 64 }).required(),
      },
      primaryKey: ["id"],
    });
  },
};
`;

const DATA_MIGRATION = `import { table } from "@zeroship/migrate";

export const name = "seed_notes";

export default {
  data() {
    table("notes").insert({ rows: { id: 1, body: "written by the CLI" } });
  },
  inverse() {
    table("notes").delete({ where: (col) => col("id").eq(1) });
  },
};
`;

function writeMigrations(migrations: string): void {
  writeFileSync(join(migrations, "20260101000000_create_notes.ts"), SCHEMA_MIGRATION);
  writeFileSync(join(migrations, "20260101000001_seed_notes.ts"), DATA_MIGRATION);
}

test("the CLI applies to a SQLite file and the rows are really there", () => {
  // Inside the test directory, not the system temp dir: the migration imports
  // "@zeroship/migrate", which only resolves from within the workspace. That is a
  // property of the unpublished source checkout, not a defect.
  const work = mkdtempSync(join(HERE, "sqlite-cli-"));
  const dbPath = join(work, "app.db");
  try {
    const migrations = join(work, "migrations");
    writeFileSync(join(work, "policy.toml"), CHARTER);
    writeFileSync(join(work, "registry.json"), JSON.stringify({ notes: OWNER_APP }));
    mkdirSync(migrations);
    writeMigrations(migrations);

    const applied = spawnCli(
      [
        "apply",
        "--dir",
        migrations,
        "--database-url",
        `sqlite:${dbPath}`,
        "--policy",
        join(work, "policy.toml"),
        "--registry",
        join(work, "registry.json"),
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

test("the CLI rolls back a SQLite file and the reversed row is really gone", () => {
  const work = mkdtempSync(join(HERE, "sqlite-cli-rollback-"));
  const dbPath = join(work, "app.db");
  try {
    const migrations = join(work, "migrations");
    writeFileSync(join(work, "policy.toml"), CHARTER);
    writeFileSync(join(work, "registry.json"), JSON.stringify({ notes: OWNER_APP }));
    mkdirSync(migrations);
    writeMigrations(migrations);

    const shared = [
      "--dir",
      migrations,
      "--database-url",
      `sqlite:${dbPath}`,
      "--policy",
      join(work, "policy.toml"),
      "--registry",
      join(work, "registry.json"),
      "--schema",
      "main",
      "--owner-app",
      OWNER_APP,
    ];

    const applied = spawnCli(["apply", ...shared], work);
    assert.equal(
      applied.status,
      0,
      `apply must succeed; stdout=${applied.stdout} stderr=${applied.stderr}`,
    );

    const rolledBack = spawnCli(["rollback", ...shared, "--steps", "1", "--approve"], work);
    assert.equal(
      rolledBack.status,
      0,
      `rollback must succeed; stdout=${rolledBack.stdout} stderr=${rolledBack.stderr}`,
    );
    assert.match(
      rolledBack.stdout,
      /rollback: 1 rolled back/,
      `one step was asked for; stdout=${rolledBack.stdout}`,
    );

    // The FILE again, not the reply. The authored `inverse()` ran, and only it: the
    // table the first migration created is still there, because one step was asked
    // for and the engine unwound one.
    const db = new DatabaseSync(dbPath);
    try {
      const tables = db
        .prepare("SELECT name FROM sqlite_master WHERE type = ?")
        .all("table")
        .map((row: Record<string, unknown>) => row.name as string);
      assert.ok(tables.includes("notes"), `the table survives one step; saw ${tables.join(",")}`);

      const rows = db.prepare("SELECT id FROM notes").all() as Array<Record<string, unknown>>;
      assert.equal(rows.length, 0, "the seeded row was removed by the authored inverse");
    } finally {
      db.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

// A WAL application database comes back from an apply still in WAL.
//
// Journal mode is the one effect zero-migrate has on a SQLite database that
// OUTLIVES the migration: it is a property of the connection profile, so it lands
// even on an apply that changes nothing, and it does not revert when the
// connection closes. WAL is the mode the engine opens with, so a database already
// in it is left alone.
//
// This arm is the promise an operator plans around - "your WAL database is still
// WAL afterwards" - and it is the one that fails if the profile ever pins
// something else again.
//
// The assertions read the FILE with a separate connection after the CLI process has
// exited. A pragma read on the engine's own connection would only be the engine
// agreeing with itself, and the claim is specifically about persistence.

test("a WAL application database is left in WAL, persistently", () => {
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
    writeFileSync(join(work, "registry.json"), JSON.stringify({ notes: OWNER_APP }));
    mkdirSync(migrations);
    writeMigrations(migrations);

    const applied = spawnCli(
      [
        "apply",
        "--dir",
        migrations,
        "--database-url",
        `sqlite:${dbPath}`,
        "--policy",
        join(work, "policy.toml"),
        "--registry",
        join(work, "registry.json"),
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
        "wal",
        "the apply must leave the application database in WAL",
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

// `plan` is advertised as a dry run, and what it does to the FILE is a separate
// question from what it does to the SCHEMA.
//
// `statusIr` under an in-process driver opens through the same
// `SqliteBackend::open` as apply, so `plan` gets the same connection profile and
// sets `journal_mode = WAL` before it reads anything; the `readOnly` flag reaches
// only the journal-bootstrap decision, not the connection. A database already in
// WAL is therefore untouched, which this arm pins.
//
// The RESIDUAL, pinned below beside it: a database in the rollback-journal mode is
// converted to WAL by a preview command. That is a real header write from a dry
// run. It is smaller than it looks - WAL is the mode every other database the
// platform opens uses, and it is the direction the runtime wants anyway rather
// than away from it - but it is a write, and it is recorded here rather than
// assumed. A genuinely read-only open for the status path would close it.

test("plan, a dry run, leaves a WAL application database in WAL", () => {
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
    writeFileSync(join(work, "registry.json"), JSON.stringify({ notes: OWNER_APP }));
    mkdirSync(migrations);
    writeMigrations(migrations);

    const planned = spawnCli(
      [
        "plan",
        "--dir",
        migrations,
        "--database-url",
        `sqlite:${dbPath}`,
        "--policy",
        join(work, "policy.toml"),
        "--registry",
        join(work, "registry.json"),
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
      /would apply 2 migrations/,
      "plan must have previewed both pending migrations",
    );

    const after = new DatabaseSync(dbPath);
    try {
      const mode = after.prepare("PRAGMA journal_mode").get() as Record<string, unknown>;
      assert.equal(
        String(mode?.journal_mode).toLowerCase(),
        "wal",
        "a dry run must leave a WAL database in WAL",
      );

      // And it really was only a preview - nothing was applied. Without this the
      // arm could not tell "plan previewed" from "plan applied".
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

// The residual, stated as a measurement rather than as a promise: a preview
// command writes the journal mode of a database that was NOT already in WAL.
test("plan, a dry run, still converts a rollback-journal database to WAL", () => {
  const work = mkdtempSync(join(HERE, "sqlite-plan-delete-"));
  const dbPath = join(work, "app.db");
  try {
    const before = new DatabaseSync(dbPath);
    try {
      before.exec("PRAGMA journal_mode = DELETE");
      before.exec("CREATE TABLE wal_probe (id INTEGER PRIMARY KEY)");
    } finally {
      before.close();
    }
    // Prove the fixture took, or the assertion below measures nothing.
    const reopened = new DatabaseSync(dbPath);
    try {
      assert.equal(
        String(
          (reopened.prepare("PRAGMA journal_mode").get() as Record<string, unknown>)
            ?.journal_mode,
        ).toLowerCase(),
        "delete",
        "the fixture must start from a rollback-journal database",
      );
    } finally {
      reopened.close();
    }

    const migrations = join(work, "migrations");
    writeFileSync(join(work, "policy.toml"), CHARTER);
    writeFileSync(join(work, "registry.json"), JSON.stringify({ notes: OWNER_APP }));
    mkdirSync(migrations);
    writeMigrations(migrations);

    const planned = spawnCli(
      [
        "plan",
        "--dir",
        migrations,
        "--database-url",
        `sqlite:${dbPath}`,
        "--policy",
        join(work, "policy.toml"),
        "--registry",
        join(work, "registry.json"),
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
    assert.match(
      planned.stdout,
      /would apply 2 migrations/,
      "plan must have previewed both pending migrations",
    );

    const after = new DatabaseSync(dbPath);
    try {
      assert.equal(
        String(
          (after.prepare("PRAGMA journal_mode").get() as Record<string, unknown>)?.journal_mode,
        ).toLowerCase(),
        "wal",
        "TODAY a dry run converts a rollback-journal database to WAL; when a read-only open lands this must read 'delete'",
      );
      const applied = after
        .prepare("SELECT name FROM sqlite_master WHERE type = ? AND name = ?")
        .all("table", "notes");
      assert.equal(applied.length, 0, "and it still must not have created the previewed table");
    } finally {
      after.close();
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
